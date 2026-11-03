//! P3.3 companions: Drop Basket (`--basket`) and Mini status (`--mini`).
//!
//! - Basket: tiny always-on-top window. Paste a URL + Add, or DROP `.txt`
//!   (one URL per line) / `.url` files onto it — dropped links queue
//!   straight into the shared `queue.json`.
//! - Mini: compact live status (active count, total speed, overall bar) with
//!   Open-main / Pause-all / Resume / Close. Orders travel via `cmd.json`
//!   (`pause_all` / `resume_all`), consumed by the main app.

use crate::basket_service::{launch_main, queue_one, read_queue, send_cmd};
pub(crate) use crate::basket_service::{
    gpui_launch_main, gpui_queue_items, gpui_queue_one, gpui_send_cmd, mini_snapshot,
    urls_from_dropped, MiniSnapshot,
};
use crate::model::DownloadItem;

fn centered_window(title: &str, w: f32, h: f32) -> eframe::NativeOptions {
    let mut viewport = egui::ViewportBuilder::default()
        .with_title(title)
        .with_inner_size([w, h])
        .with_min_inner_size([w - 40.0, h - 40.0])
        .with_always_on_top()
        .with_resizable(true);
    if let Ok(icon) = eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon.png")) {
        viewport = viewport.with_icon(std::sync::Arc::new(icon));
    }
    eframe::NativeOptions { viewport, ..Default::default() }
}

fn center_on_first(ctx: &egui::Context, done: &mut bool, w: f32, h: f32) {
    if *done {
        return;
    }
    // Retries until the monitor geometry is known, then lands in the middle of
    // the work area (taskbar excluded).
    if crate::model::center_window(ctx, egui::vec2(w, h)) {
        *done = true;
    }
}

// ---------------------------------------------------------------- basket ---

struct BasketApp {
    url: String,
    notice: String,
    centered: bool,
    items: Vec<DownloadItem>,
    last: std::time::Instant,
}

impl BasketApp {
    fn refresh(&mut self) {
        self.items = read_queue();
        self.last = std::time::Instant::now();
    }

    fn ingest_drops(&mut self, ctx: &egui::Context) {
        let dropped: Vec<std::path::PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .filter_map(|f| f.path.clone())
                .collect()
        });
        if dropped.is_empty() {
            return;
        }
        let mut n = 0;
        let mut msgs = Vec::new();
        for p in dropped {
            if p.is_dir() {
                continue;
            }
            for u in urls_from_dropped(&p) {
                let r = queue_one(&u);
                if r == "queued" {
                    n += 1;
                } else if msgs.len() < 3 {
                    msgs.push(format!("{}: {r}", p.display()));
                }
            }
        }
        self.notice = if n > 0 {
            format!("Queued {n} link(s) from dropped files.")
        } else if msgs.is_empty() {
            "No URLs found in dropped files (.txt / .url).".to_string()
        } else {
            msgs.join("; ")
        };
        self.refresh();
    }
}

/// Entry point for `--basket`.
pub fn run_basket() -> eframe::Result<()> {
    let mut app = BasketApp {
        url: String::new(),
        notice: "Paste a URL, or drop .txt / .url files here.".to_string(),
        centered: false,
        items: Vec::new(),
        last: std::time::Instant::now() - std::time::Duration::from_secs(10),
    };
    app.refresh();
    eframe::run_native(
        "Drop Basket",
        centered_window("Drop Basket", 420.0, 520.0),
        Box::new(|cc| {
            crate::app::setup_fonts(cc);
            // Spawned as its own process: pick up whatever theme `ui.json`
            // holds (written by the main app / the Settings window).
            crate::app::apply_theme(&cc.egui_ctx, &crate::app::stored_theme());
            Ok(Box::new(app))
        }),
    )
}

impl eframe::App for BasketApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        center_on_first(ctx, &mut self.centered, 420.0, 520.0);
        self.ingest_drops(ctx);
        if self.last.elapsed() >= std::time::Duration::from_secs(2) {
            self.refresh();
        }
        ctx.request_repaint_after(std::time::Duration::from_secs(1));
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("Drop Basket");
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.url)
                        .hint_text("https://…")
                        .desired_width(f32::INFINITY),
                );
                if ui.button("Add").clicked() && !self.url.trim().is_empty() {
                    self.notice = queue_one(&self.url).to_string();
                    if self.notice == "queued" {
                        self.url.clear();
                    }
                    self.refresh();
                }
            });
            ui.small(&self.notice);
            ui.separator();
            ui.label(format!("{} item(s) in queue", self.items.len()));
            egui::ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
                let mut rev: Vec<&DownloadItem> = self.items.iter().collect();
                rev.reverse();
                for d in rev.iter().take(12) {
                    ui.horizontal(|ui| {
                        ui.label(d.status.label());
                        ui.monospace(d.filename.clone());
                    });
                }
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Close").clicked() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });
        });
    }
}

// ------------------------------------------------------------------ mini ---

struct MiniApp {
    centered: bool,
    active: usize,
    queued: usize,
    speed: f64,
    frac: f32,
}

/// Entry point for `--mini`.
pub fn run_mini() -> eframe::Result<()> {
    let mut app = MiniApp { centered: false, active: 0, queued: 0, speed: 0.0, frac: 0.0 };
    refresh_mini(&mut app);
    eframe::run_native(
        "Download Craft Mini",
        centered_window("Download Craft Mini", 340.0, 230.0),
        Box::new(|cc| {
            crate::app::setup_fonts(cc);
            // Spawned as its own process: pick up whatever theme `ui.json`
            // holds (written by the main app / the Settings window).
            crate::app::apply_theme(&cc.egui_ctx, &crate::app::stored_theme());
            Ok(Box::new(app))
        }),
    )
}

fn refresh_mini(app: &mut MiniApp) {
    let snapshot = mini_snapshot();
    app.active = snapshot.active;
    app.queued = snapshot.queued;
    app.speed = snapshot.speed;
    app.frac = snapshot.frac;
}

impl eframe::App for MiniApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        center_on_first(ctx, &mut self.centered, 340.0, 230.0);
        refresh_mini(self);
        ctx.request_repaint_after(std::time::Duration::from_secs(1));
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading(format!("{} active · {} queued", self.active, self.queued));
            ui.label(if self.speed > 0.0 {
                format!("{}/s", crate::model::format_bytes(self.speed as u64))
            } else {
                "-".to_string()
            });
            ui.add(egui::ProgressBar::new(self.frac).desired_height(10.0).show_percentage());
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui.button("Open main").clicked() {
                    launch_main();
                }
                if ui.button("Pause all").clicked() {
                    send_cmd("pause_all");
                }
                if ui.button("Resume").clicked() {
                    send_cmd("resume_all");
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Close").clicked() {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                });
            });
        });
    }
}
