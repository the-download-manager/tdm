//! TDM desktop GUI - a classic download-manager main window: menu bar, icon toolbar,
//! category tree, download list with the familiar column set, and status bar. The
//! dialogs and engine plumbing are TDM's own.

use crate::config::Config;
use crate::engine::{Engine, Progress};
use crate::http::filename_from_url;
use crate::model::{Download, Status};
use crate::store::Store;
use crate::util::{category_for, ellipsize, human_bytes, human_duration, human_speed, parse_size, sanitize_filename};
use anyhow::Result;
use eframe::egui;
use egui::{Align, Align2, Color32, FontId, Layout, RichText, Rounding, Stroke};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

// ---------------------------------------------------------------------------
// palette (classic download-manager window)
// ---------------------------------------------------------------------------

const BG: Color32 = Color32::from_rgb(240, 240, 240);
const MENU_BG: Color32 = Color32::from_rgb(240, 240, 240);
const TOOLBAR_TOP: Color32 = Color32::from_rgb(250, 251, 253);
const TOOLBAR_BOTTOM: Color32 = Color32::from_rgb(228, 233, 241);
const PANEL_BG: Color32 = Color32::from_rgb(247, 248, 250);
const HEADER_TOP: Color32 = Color32::from_rgb(240, 244, 250);
const HEADER_BOTTOM: Color32 = Color32::from_rgb(208, 216, 230);
const BORDER: Color32 = Color32::from_rgb(150, 160, 175);
const GRID: Color32 = Color32::from_rgb(214, 219, 228);
const TEXT: Color32 = Color32::from_rgb(20, 20, 20);
const DIM: Color32 = Color32::from_rgb(110, 110, 110);
const SELECTED: Color32 = Color32::from_rgb(49, 106, 197);
const GREEN: Color32 = Color32::from_rgb(0, 128, 0);
const RED: Color32 = Color32::from_rgb(190, 30, 30);
const ORANGE: Color32 = Color32::from_rgb(190, 110, 0);
const BLUE: Color32 = Color32::from_rgb(0, 84, 166);

const ROW_H: f32 = 20.0;
const HEADER_H: f32 = 24.0;
/// Minimum readable width for each column: name, Q, size, status, time left,
/// transfer rate, last try, description.
const MIN_COLS: [f32; 8] = [140.0, 22.0, 100.0, 92.0, 80.0, 100.0, 95.0, 100.0];
const PROJECT_URL: &str = "https://github.com/the-download-manager/tdm";
const LICENSE_URL: &str = "https://github.com/the-download-manager/tdm/blob/main/LICENSE";
const ISSUES_URL: &str = "https://github.com/the-download-manager/tdm/issues";
const NOTICE_URL: &str =
    "https://github.com/the-download-manager/tdm/blob/main/NOTICE.md";

#[derive(Clone, PartialEq, Eq)]
enum Filter {
    All,
    Unfinished,
    Finished,
    Category(String),
}

struct AddDialog {
    url: String,
    dir: String,
    name: String,
    connections: u32,
    error: Option<String>,
    focus_url: bool,
}

struct ProgressWindow {
    slot: usize,
    displayed: f32,
    phase: f32,
}



struct SpeedSample {
    bytes: u64,
    at: Instant,
    speed: u64,
    changed: Instant,
}

struct AppState {
    progress: Mutex<HashMap<String, Arc<Progress>>>,
    tokens: Mutex<HashMap<String, CancellationToken>>,
}

pub struct TdmApp {
    store: Store,
    cfg: Config,
    engine: Arc<Engine>,
    rt: tokio::runtime::Runtime,
    state: Arc<AppState>,
    records: Vec<Download>,
    selected: HashSet<String>,
    selection_anchor: Option<String>,
    filter: Filter,
    add: Option<AddDialog>,
    options: Option<Config>,
    details: Option<String>,
    scheduler: bool,
    about: bool,
    register_joke: bool,
    flag_texture: Option<egui::TextureHandle>,
    logo_texture: Option<egui::TextureHandle>,
    show_toolbar: bool,
    show_categories: bool,
    show_status: bool,
    status_msg: String,
    last_load: Instant,
    total_speed: u64,
    queue_running: bool,
    /// last speed sample for downloads started outside the GUI
    speed_samples: HashMap<String, SpeedSample>,
    progress_windows: HashMap<String, ProgressWindow>,
    dismissed_progress: HashSet<String>,
    next_progress_slot: usize,
    allow_exit: bool,
    startup: bool,
    minimized_hidden: bool,
    start_hidden: bool,
    column_widths: [f32; 8],
    columns_ready: bool,
    confirm_delete_files: Option<Vec<String>>,
    resize_drag: Option<usize>,
    resize_start_x: f32,
    resize_start_left: f32,
    resize_start_next: f32,
    resize_start_desc: f32,
}

impl TdmApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        apply_theme(&cc.egui_ctx);
        let cfg = Config::load();
        let store = Store::new();
        let _ = store.ensure();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        let engine = Arc::new(Engine::new(Arc::new(cfg.clone()), store.clone()).expect("engine"));
        let api_port = cfg.api_port;
        rt.spawn(async move {
            if let Err(err) = crate::server::run(Some(api_port)).await {
                let dir = crate::config::Config::home();
                let _ = std::fs::create_dir_all(&dir);
                let _ = std::fs::write(dir.join("api-error.log"), format!("{:#}\n", err));
            }
        });
        let mut records = store.load_all();
        for dl in records.iter_mut() {
            if dl.status.is_active() {
                dl.status = Status::Paused;
                let _ = store.save(dl);
            }
        }
        #[cfg(windows)]
        spawn_tray_thread();
        let saved_columns = cfg.column_widths;
        Self {
            store,
            cfg,
            engine,
            rt,
            state: Arc::new(AppState {
                progress: Mutex::new(HashMap::new()),
                tokens: Mutex::new(HashMap::new()),
            }),
            records,
            selected: HashSet::new(),
            selection_anchor: None,
            filter: Filter::All,
            add: None,
            options: None,
            details: None,
            scheduler: false,
            about: std::env::var("TDM_ABOUT").is_ok(),
            register_joke: false,
            flag_texture: None,
            logo_texture: None,
            show_toolbar: true,
            show_categories: true,
            show_status: true,
            status_msg: "Ready".into(),
            last_load: Instant::now(),
            total_speed: 0,
            queue_running: false,
            speed_samples: HashMap::new(),
            progress_windows: HashMap::new(),
            dismissed_progress: HashSet::new(),
            next_progress_slot: 0,
            allow_exit: false,
            startup: startup_enabled(),
            minimized_hidden: false,
            start_hidden: std::env::args().any(|arg| arg == "--tray" || arg == "--startup"),
            column_widths: saved_columns.unwrap_or([0.0; 8]),
            columns_ready: saved_columns.is_some(),
            confirm_delete_files: None,
            resize_drag: None,
            resize_start_x: 0.0,
            resize_start_left: 0.0,
            resize_start_next: 0.0,
            resize_start_desc: 0.0,
        }
    }

    fn refresh(&mut self, force: bool) {
        if force || self.last_load.elapsed() > Duration::from_millis(500) {
            self.records = self.store.load_all();
            self.last_load = Instant::now();

            // active downloads started by the server/extension are not in the GUI's
            // progress map, so derive speed from the record's downloaded counter
            let now = Instant::now();
            let mut updates = Vec::new();
            let mut active = Vec::new();
            for dl in &self.records {
                if matches!(dl.status, Status::Downloading | Status::Connecting) {
                    active.push(dl.id.clone());
                    let mut changed = now;
                    let speed = match self.speed_samples.get(&dl.id) {
                        Some(prev) => {
                            let dt = now.duration_since(prev.at).as_secs_f64();
                            if dl.downloaded > prev.bytes {
                                changed = now;
                                let instant =
                                    (dl.downloaded - prev.bytes) as f64 / dt.max(0.001);
                                if prev.speed > 0 {
                                    (prev.speed as f64 * 0.6 + instant * 0.4) as u64
                                } else {
                                    instant as u64
                                }
                            } else if now.duration_since(prev.changed)
                                < Duration::from_secs(2)
                            {
                                // hold the last rate briefly between progress bursts
                                prev.speed
                            } else {
                                (prev.speed as f64 * 0.85) as u64
                            }
                        }
                        None => 0,
                    };
                    updates.push((dl.id.clone(), dl.downloaded, now, speed, changed));
                }
            }
            self.speed_samples.retain(|id, _| active.contains(id));
            for (id, bytes, at, speed, changed) in updates {
                self.speed_samples.insert(
                    id,
                    SpeedSample {
                        bytes,
                        at,
                        speed,
                        changed,
                    },
                );
            }

            self.open_progress_windows();
        }
    }

    fn open_progress_windows(&mut self) {
        let mut to_open = Vec::new();
        for dl in &self.records {
            if dl.status.is_active() {
                if !self.progress_windows.contains_key(&dl.id)
                    && !self.dismissed_progress.contains(&dl.id)
                {
                    to_open.push(dl.id.clone());
                }
            } else {
                self.dismissed_progress.remove(&dl.id);
            }
        }
        for id in to_open {
            let slot = self.next_progress_slot;
            self.next_progress_slot += 1;
            self.progress_windows.insert(
                id,
                ProgressWindow {
                    slot,
                    displayed: 0.0,
                    phase: 0.0,
                },
            );
        }
    }

    fn start(&mut self, id: String) {
        if let Some(dl) = self.store.load(&id) {
            match dl.status {
                Status::Completed => {
                    self.status_msg = "That download is already completed".into();
                    return;
                }
                _ if self.is_running(&id) => return,
                _ => {}
            }
        }
        let store = self.store.clone();
        let engine = self.engine.clone();
        let progress = Progress::new();
        let ctl = CancellationToken::new();
        {
            let mut p = self.state.progress.lock().unwrap();
            p.insert(id.clone(), progress.clone());
            let mut t = self.state.tokens.lock().unwrap();
            t.insert(id.clone(), ctl.clone());
        }
        let state = self.state.clone();
        self.rt.spawn(async move {
            let mut dl = match store.load(&id) {
                Some(d) => d,
                None => return,
            };
            engine.run(&mut dl, ctl, progress).await;
            state.progress.lock().unwrap().remove(&id);
            state.tokens.lock().unwrap().remove(&id);
        });
    }

    fn stop(&mut self, id: &str) {
        if let Some(token) = self.state.tokens.lock().unwrap().get(id) {
            token.cancel();
            self.status_msg = "Pausing...".into();
        }
    }

    fn stop_all(&mut self) {
        for (_, token) in self.state.tokens.lock().unwrap().iter() {
            token.cancel();
        }
        self.status_msg = "Stopping all downloads...".into();
    }

    fn select_only(&mut self, id: &str) {
        self.selected.clear();
        self.selected.insert(id.to_string());
        self.selection_anchor = Some(id.to_string());
    }

    fn toggle_select(&mut self, id: &str) {
        if self.selected.remove(id) {
            if self.selection_anchor.as_deref() == Some(id) {
                self.selection_anchor = self.selected.iter().next().cloned();
            }
        } else {
            self.selected.insert(id.to_string());
            self.selection_anchor = Some(id.to_string());
        }
    }

    fn select_range(&mut self, id: &str) {
        let ids: Vec<String> = self.visible().iter().map(|d| d.id.clone()).collect();
        let Some(target) = ids.iter().position(|x| x == id) else {
            return;
        };
        let anchor = self
            .selection_anchor
            .as_ref()
            .and_then(|a| ids.iter().position(|x| x == a))
            .unwrap_or(target);
        let (lo, hi) = if anchor <= target {
            (anchor, target)
        } else {
            (target, anchor)
        };
        self.selected.clear();
        for item in &ids[lo..=hi] {
            self.selected.insert(item.clone());
        }
    }

    fn select_all_visible(&mut self) {
        self.selected = self.visible().iter().map(|d| d.id.clone()).collect();
        self.selection_anchor = self.selected.iter().next().cloned();
    }

    fn selected_ids(&self) -> Vec<String> {
        self.visible()
            .iter()
            .filter(|d| self.selected.contains(&d.id))
            .map(|d| d.id.clone())
            .collect()
    }

    fn primary_selected(&self) -> Option<String> {
        self.selection_anchor
            .as_ref()
            .filter(|id| self.selected.contains(*id))
            .cloned()
            .or_else(|| self.selected_ids().into_iter().next())
    }

    fn remove_many(&mut self, ids: &[String]) {
        if ids.is_empty() {
            return;
        }
        for id in ids {
            self.stop(id);
            if let Some(dl) = self.store.resolve(id) {
                let _ = self.store.delete(&dl.id);
            }
        }
        self.selected.clear();
        self.selection_anchor = None;
        self.refresh(true);
        self.status_msg = if ids.len() == 1 {
            "Download removed".into()
        } else {
            format!("{} downloads removed", ids.len())
        };
    }

    fn delete_files_and_records(&mut self, ids: &[String]) {
        let mut removed = 0usize;
        for id in ids {
            self.stop(id);
            if let Some(dl) = self.store.resolve(id) {
                let final_path = dl.final_path();
                let part_path = dl.part_path();
                let work_dir = final_path.with_extension("parts");
                if work_dir.is_dir() {
                    let _ = std::fs::remove_dir_all(&work_dir);
                }
                if final_path.exists() {
                    let _ = std::fs::remove_file(&final_path);
                }
                if part_path.exists() {
                    let _ = std::fs::remove_file(&part_path);
                }
                let _ = self.store.delete(&dl.id);
                removed += 1;
            }
        }
        self.selected.clear();
        self.selection_anchor = None;
        self.refresh(true);
        self.status_msg = if removed == 1 {
            "Download and file deleted".into()
        } else {
            format!("{removed} downloads and files deleted")
        };
    }

    fn add_download(&mut self, start_now: bool) {
        let Some(dialog) = self.add.take() else { return };
        let url = dialog.url.trim().to_string();
        if url.is_empty() {
            self.add = Some(AddDialog { error: Some("Enter a URL".into()), ..dialog });
            return;
        }
        if let Err(e) = url::Url::parse(&url) {
            self.add = Some(AddDialog { error: Some(format!("Invalid URL: {e}")), ..dialog });
            return;
        }

        let name = if dialog.name.trim().is_empty() {
            sanitize_filename(&filename_from_url(&url).unwrap_or_else(|| "download.bin".into()))
        } else {
            sanitize_filename(dialog.name.trim())
        };
        let name_locked = !dialog.name.trim().is_empty();
        let mut dir = if dialog.dir.trim().is_empty() {
            self.cfg.download_dir.clone()
        } else {
            dialog.dir.trim().to_string()
        };
        if self.cfg.categories && !name_locked {
            dir = format!("{}/{}", dir.trim_end_matches(['/', '\\']), category_for(&name));
        }

        let mut dl = Download::new(url, dir, name, dialog.connections);
        dl.name_locked = name_locked;
        if let Err(e) = self.store.save(&dl) {
            self.status_msg = format!("Could not save: {e}");
            return;
        }
        self.status_msg = format!("Added {}", ellipsize(&dl.file_name, 50));
        self.select_only(&dl.id.clone());
        if start_now {
            self.start(dl.id.clone());
        }
        self.refresh(true);
    }

    fn visible(&self) -> Vec<Download> {
        let mut items: Vec<Download> = self
            .records
            .iter()
            .filter(|d| match &self.filter {
                Filter::All => true,
                Filter::Unfinished => d.status != Status::Completed,
                Filter::Finished => d.status == Status::Completed,
                Filter::Category(c) => &d.category == c,
            })
            .cloned()
            .collect();
        items.sort_by_key(|d| std::cmp::Reverse(d.created_at));
        items
    }

    fn speed_of(&self, id: &str) -> u64 {
        if let Some(progress) = self.state.progress.lock().unwrap().get(id) {
            return progress.speed();
        }
        self.speed_samples.get(id).map(|s| s.speed).unwrap_or(0)
    }

    fn done_of(&self, dl: &Download) -> u64 {
        self.state
            .progress
            .lock()
            .unwrap()
            .get(&dl.id)
            .map(|p| p.done())
            .unwrap_or(dl.downloaded)
    }

    fn is_running(&self, id: &str) -> bool {
        self.state.tokens.lock().unwrap().contains_key(id)
    }

    fn progress_windows_ui(&mut self, ctx: &egui::Context) {
        let ids: Vec<String> = self.progress_windows.keys().cloned().collect();
        for id in ids {
            let Some(dl) = self.records.iter().find(|d| d.id == id).cloned() else {
                self.progress_windows.remove(&id);
                continue;
            };
            let done = self.done_of(&dl);
            let speed = self.speed_of(&id);
            let pct = if dl.size > 0 {
                (done as f64 / dl.size as f64 * 100.0).min(100.0)
            } else {
                0.0
            };
            let indeterminate = dl.size == 0 && dl.status.is_active();
            let eta = if speed > 0 && dl.size > done {
                (dl.size - done) / speed
            } else {
                0
            };
            let animate = matches!(
                dl.status,
                Status::Queued | Status::Connecting | Status::Downloading
            );
            let completed = dl.status == Status::Completed || (dl.size > 0 && done >= dl.size);
            // ease the drawn fill toward the real percentage: the bar visibly
            // "drags" behind and catches up faster as the transfer completes
            let (displayed, anim_phase) = {
                let Some(window) = self.progress_windows.get_mut(&id) else {
                    continue;
                };
                let dt = ctx.input(|i| i.stable_dt).clamp(0.0, 0.1);
                let target = pct as f32 / 100.0;
                if !animate || completed {
                    window.displayed = target;
                } else if indeterminate {
                    window.displayed = 0.36;
                } else {
                    let rate = 0.55 + 4.2 * target.powf(1.7);
                    window.displayed += (target - window.displayed) * (1.0 - (-rate * dt).exp());
                    if (target - window.displayed).abs() < 0.0005 {
                        window.displayed = target;
                    }
                }
                // accumulate phase instead of multiplying absolute time by a
                // changing rate, which made the loop visibly jump/reset
                if animate && !completed {
                    let anim_rate =
                        0.22 + 1.6 * target.powf(1.8) + (speed as f32 / 40_000_000.0).min(0.2);
                    window.phase = (window.phase + dt * anim_rate).rem_euclid(1.0);
                }
                (window.displayed, window.phase)
            };
            let slot = self.progress_windows.get(&id).map(|w| w.slot).unwrap_or(0);
            let title = format!("TDM — {} — {:.0}%", ellipsize(&dl.file_name, 24), pct);
            let pos = egui::pos2(150.0 + slot as f32 * 26.0, 120.0 + slot as f32 * 26.0);
            let mut window_open = true;
            let mut close_clicked = false;

            egui::Window::new(title)
                .id(egui::Id::new(("tdm-download-progress", id.clone())))
                .default_pos(pos)
                .default_width(470.0)
                .resizable(false)
                .collapsible(false)
                .open(&mut window_open)
                .show(ctx, |ui| {
                    ui.set_min_width(450.0);
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new(ellipsize(&dl.file_name, 56))
                                .size(14.0)
                                .strong()
                                .color(TEXT),
                        );
                    });
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("Save to:").size(11.5).strong());
                        ui.label(RichText::new(ellipsize(&dl.dir, 52)).size(11.0).color(DIM));
                        if ui.small_button("Open folder").clicked() {
                            let _ = std::process::Command::new("explorer").arg(&dl.dir).spawn();
                        }
                    });
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new(format!(
                                "Time left: {}",
                                if eta > 0 {
                                    human_duration(eta)
                                } else {
                                    "—".to_string()
                                }
                            ))
                            .size(12.0),
                        );
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            let text = if speed > 0 {
                                human_speed(speed)
                            } else {
                                "—".to_string()
                            };
                            ui.label(
                                RichText::new(text)
                                    .size(24.0)
                                    .strong()
                                    .family(egui::FontFamily::Monospace)
                                    .color(BLUE),
                            );
                            ui.label(RichText::new("Speed:").size(12.0));
                        });
                    });
                    ui.add_space(4.0);
                    let label = if dl.size > 0 {
                        format!("{} of {}", human_bytes(done), human_bytes(dl.size))
                    } else {
                        format!("{} received", human_bytes(done))
                    };
                    draw_rainbow_bar(ui, displayed, indeterminate, animate, anim_phase, &label);
                    ui.add_space(4.0);
                    let (status_text, status_color) = match dl.status {
                        Status::Queued => ("Queued".to_string(), DIM),
                        Status::Connecting => ("Connecting ...".to_string(), BLUE),
                        Status::Downloading => (format!("Downloading ... {:.0}%", pct), BLUE),
                        Status::Paused => (format!("Paused at {:.0}%", pct), ORANGE),
                        Status::Completed => ("Complete".to_string(), GREEN),
                        Status::Error => (
                            format!("Error: {}", dl.error.as_deref().unwrap_or("download failed")),
                            RED,
                        ),
                        Status::Canceled => ("Canceled".to_string(), RED),
                    };
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new(ellipsize(&status_text, 70))
                                .size(12.0)
                                .strong()
                                .color(status_color),
                        );
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if ui.button("Close").clicked() {
                                close_clicked = true;
                            }
                        });
                    });
                    if animate {
                        ui.ctx().request_repaint();
                    }
                });

            if !window_open || close_clicked {
                self.progress_windows.remove(&id);
                self.dismissed_progress.insert(id);
            }
        }
    }

    fn handle_window_state(&mut self, ctx: &egui::Context) {
        let close = ctx.input(|i| i.viewport().close_requested());
        if close && !self.allow_exit {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        }
        let minimized = ctx.input(|i| i.viewport().minimized.unwrap_or(false));
        if minimized && !self.allow_exit {
            if !self.minimized_hidden {
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
                self.minimized_hidden = true;
            }
        } else {
            self.minimized_hidden = false;
        }
    }

    fn delete_completed(&mut self) {
        let done: Vec<String> = self
            .records
            .iter()
            .filter(|d| d.status == Status::Completed)
            .map(|d| d.id.clone())
            .collect();
        for id in &done {
            let _ = self.store.delete(id);
        }
        self.refresh(true);
        self.status_msg = format!("Deleted {} completed downloads", done.len());
    }
}

fn apply_theme(ctx: &egui::Context) {
    // Tahoma keeps the typography close to the classic look
    let mut fonts = egui::FontDefinitions::default();
    if let Ok(bytes) = std::fs::read("C:\\Windows\\Fonts\\tahoma.ttf") {
        fonts
            .font_data
            .insert("tahoma".to_owned(), egui::FontData::from_owned(bytes));
        if let Some(family) = fonts.families.get_mut(&egui::FontFamily::Proportional) {
            family.insert(0, "tahoma".to_owned());
        }
    }
    ctx.set_fonts(fonts);

    let mut style = (*ctx.style()).clone();
    style.visuals = egui::Visuals::light();
    style.visuals.panel_fill = BG;
    style.visuals.window_fill = Color32::from_rgb(246, 246, 246);
    style.visuals.extreme_bg_color = Color32::WHITE;
    style.visuals.selection.bg_fill = SELECTED;
    style.visuals.selection.stroke = Stroke::new(1.0_f32, Color32::WHITE);
    style.visuals.widgets.inactive.fg_stroke = Stroke::new(1.0_f32, Color32::from_rgb(35, 35, 35));
    style.visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0_f32, Color32::from_rgb(35, 35, 35));
    style.spacing.item_spacing = egui::vec2(4.0, 3.0);
    style.spacing.button_padding = egui::vec2(6.0, 3.0);
    style.text_styles.insert(egui::TextStyle::Body, FontId::proportional(12.0));
    style.text_styles.insert(egui::TextStyle::Button, FontId::proportional(12.0));
    style.text_styles.insert(egui::TextStyle::Small, FontId::proportional(10.5));
    ctx.set_style(style);
}

#[cfg(windows)]
fn spawn_tray_thread() {
    std::thread::spawn(|| {
        use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem};
        use tray_icon::{Icon, TrayIconBuilder, TrayIconEvent};
        use windows_sys::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            DispatchMessageW, FindWindowW, PeekMessageW, SetForegroundWindow, ShowWindow,
            TranslateMessage, MSG, PM_REMOVE, SW_RESTORE,
        };

        unsafe {
            CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED as u32);
        }

        let image = match image::load_from_memory(include_bytes!("../assets/tdm-32.png")) {
            Ok(image) => image.to_rgba8(),
            Err(_) => return,
        };
        let (width, height) = image.dimensions();
        let icon = match Icon::from_rgba(image.into_raw(), width, height) {
            Ok(icon) => icon,
            Err(_) => return,
        };

        let menu = Menu::new();
        let show_id = MenuId::new("tdm-open");
        let exit_id = MenuId::new("tdm-exit");
        let open = MenuItem::with_id(show_id.clone(), "Open TDM", true, None);
        let exit = MenuItem::with_id(exit_id.clone(), "Exit TDM", true, None);
        if menu
            .append_items(&[&open, &PredefinedMenuItem::separator(), &exit])
            .is_err()
        {
            return;
        }
        let tray = match TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(false)
            .with_tooltip("TDM — The Download Manager")
            .with_icon(icon)
            .build()
        {
            Ok(tray) => tray,
            Err(_) => return,
        };
        let _tray = tray;

        let title: Vec<u16> = "TDM - The Download Manager\0".encode_utf16().collect();
        let class: Vec<u16> = "Window Class\0".encode_utf16().collect();
        let show_window = || {
            TRAY_RESTORE.store(true, std::sync::atomic::Ordering::SeqCst);
            unsafe {
                let hwnd = FindWindowW(class.as_ptr(), title.as_ptr());
                if !hwnd.is_null() {
                    ShowWindow(hwnd, SW_RESTORE);
                    SetForegroundWindow(hwnd);
                }
            }
        };

        loop {
            // pump the tray/menu window messages on this thread
            unsafe {
                let mut msg: MSG = std::mem::zeroed();
                while PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
                    TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
            while let Ok(event) = MenuEvent::receiver().try_recv() {
                if event.id == show_id {
                    show_window();
                } else if event.id == exit_id {
                    std::process::exit(0);
                }
            }
            while let Ok(event) = TrayIconEvent::receiver().try_recv() {
                if matches!(
                    event,
                    TrayIconEvent::Click {
                        button: tray_icon::MouseButton::Left,
                        button_state: tray_icon::MouseButtonState::Up,
                        ..
                    } | TrayIconEvent::DoubleClick {
                        button: tray_icon::MouseButton::Left,
                        ..
                    }
                ) {
                    show_window();
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    });
}

#[cfg(windows)]
static TRAY_RESTORE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";

fn startup_enabled() -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let mut cmd = std::process::Command::new("reg");
        cmd.args(["query", RUN_KEY, "/v", "TDM"])
            .creation_flags(0x0800_0000)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        return matches!(cmd.status(), Ok(status) if status.success());
    }
    #[cfg(not(windows))]
    {
        false
    }
}

fn set_startup(enabled: bool) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let mut cmd = std::process::Command::new("reg");
        if enabled {
            let exe = std::env::current_exe()
                .map(|p| format!("\"{}\" --tray", p.display()))
                .unwrap_or_else(|_| "tdm.exe --tray".to_string());
            cmd.args([
                "add", RUN_KEY, "/v", "TDM", "/t", "REG_SZ", "/d", &exe, "/f",
            ]);
        } else {
            cmd.args(["delete", RUN_KEY, "/v", "TDM", "/f"]);
        }
        let _ = cmd
            .creation_flags(0x0800_0000)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    #[cfg(not(windows))]
    {
        let _ = enabled;
    }
}

impl eframe::App for TdmApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if self.start_hidden {
            // eframe shows the first frame to avoid a white flash, so hide it
            // again once the tray icon exists
            self.start_hidden = false;
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        }
        #[cfg(windows)]
        if TRAY_RESTORE.swap(false, std::sync::atomic::Ordering::SeqCst) {
            // keep the next frame from immediately hiding the restored window
            self.minimized_hidden = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        }
        self.handle_window_state(ctx);
        self.refresh(false);
        ctx.request_repaint_after(Duration::from_millis(250));

        let mut speed: u64 = {
            let p = self.state.progress.lock().unwrap();
            p.values().map(|x| x.speed()).sum()
        };
        speed += self.speed_samples.values().map(|s| s.speed).sum::<u64>();
        self.total_speed = speed;

        self.handle_shortcuts(ctx);
        self.menu_bar(ctx);
        if self.show_toolbar {
            self.toolbar(ctx);
        }
        if self.show_status {
            self.status_bar(ctx);
        }
        if self.show_categories {
            self.categories(ctx);
        }
        self.table(ctx);

        if self.add.is_some() {
            self.add_dialog(ctx);
        }
        if self.options.is_some() {
            self.options_dialog(ctx);
        }
        if self.details.is_some() {
            self.details_window(ctx);
        }
        if self.confirm_delete_files.is_some() {
            self.confirm_delete_files_dialog(ctx);
        }
        if self.scheduler {
            self.scheduler_dialog(ctx);
        }
        if self.about {
            self.about_dialog(ctx);
        }
        if self.register_joke {
            self.registration_dialog(ctx);
        }
        self.progress_windows_ui(ctx);
    }

    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [
            BG.r() as f32 / 255.0,
            BG.g() as f32 / 255.0,
            BG.b() as f32 / 255.0,
            1.0,
        ]
    }
}

impl TdmApp {
    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        let (ctrl_n, ctrl_a, esc, enter, delete, up, down, space) = ctx.input(|i| {
            (
                i.modifiers.ctrl && i.key_pressed(egui::Key::N),
                i.modifiers.ctrl && i.key_pressed(egui::Key::A),
                i.key_pressed(egui::Key::Escape),
                i.key_pressed(egui::Key::Enter),
                i.key_pressed(egui::Key::Delete),
                i.key_pressed(egui::Key::ArrowUp),
                i.key_pressed(egui::Key::ArrowDown),
                i.key_pressed(egui::Key::Space),
            )
        });
        let modal = self.add.is_some()
            || self.options.is_some()
            || self.scheduler
            || self.about
            || self.register_joke
            || self.confirm_delete_files.is_some();

        if ctrl_n && !modal {
            self.open_add();
        }
        if esc {
            self.add = None;
            self.options = None;
            self.details = None;
            self.scheduler = false;
            self.about = false;
            self.register_joke = false;
            self.confirm_delete_files = None;
        }
        if modal {
            return;
        }

        if ctrl_a {
            self.select_all_visible();
        }

        if up || down {
            let ids: Vec<String> = self.visible().iter().map(|d| d.id.clone()).collect();
            if !ids.is_empty() {
                let current = self
                    .selection_anchor
                    .as_ref()
                    .and_then(|s| ids.iter().position(|x| x == s));
                let next = match (current, down) {
                    (None, _) => 0,
                    (Some(i), true) => (i + 1).min(ids.len() - 1),
                    (Some(i), false) => i.saturating_sub(1),
                };
                self.select_only(&ids[next].clone());
            }
        }

        let Some(id) = self.primary_selected() else {
            return;
        };
        if enter && self.details.is_none() {
            self.details = Some(id.clone());
        }
        if space {
            let ids = self.selected_ids();
            for selected in ids {
                if self.is_running(&selected) {
                    self.stop(&selected);
                } else if let Some(dl) = self.records.iter().find(|d| d.id == selected) {
                    if dl.status != Status::Completed {
                        self.start(selected.clone());
                    }
                }
            }
        }
        if delete {
            let ids = self.selected_ids();
            self.remove_many(&ids);
        }
    }

    fn menu_bar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("menu")
            .exact_height(22.0)
            .frame(egui::Frame::none().fill(MENU_BG).inner_margin(egui::Margin::symmetric(4.0, 1.0)))
            .show(ctx, |ui| {
                egui::menu::bar(ui, |ui| {
                    ui.menu_button("File", |ui| {
                        if ui.button("Add URL...").clicked() {
                            self.open_add();
                            ui.close_menu();
                        }
                        ui.separator();
                        if ui.button("Exit").clicked() {
                            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                    });
                    ui.menu_button("View", |ui| {
                        let mut changed = false;
                        changed |= ui.checkbox(&mut self.show_toolbar, "Toolbar").changed();
                        changed |= ui.checkbox(&mut self.show_categories, "Categories").changed();
                        changed |= ui.checkbox(&mut self.show_status, "Status bar").changed();
                        ui.separator();
                        if ui.button("Refresh list").clicked() {
                            self.refresh(true);
                            ui.close_menu();
                        }
                        if changed {
                            ui.close_menu();
                        }
                    });
                    ui.menu_button("Downloads", |ui| {
                        if ui.button("Select All (Ctrl+A)").clicked() {
                            self.select_all_visible();
                            ui.close_menu();
                        }
                        ui.separator();
                        if ui.button("Resume").clicked() {
                            for id in self.selected_ids() {
                                if !self.is_running(&id) {
                                    if let Some(dl) = self.records.iter().find(|d| d.id == id) {
                                        if dl.status != Status::Completed {
                                            self.start(id.clone());
                                        }
                                    }
                                }
                            }
                            ui.close_menu();
                        }
                        if ui.button("Stop").clicked() {
                            for id in self.selected_ids() {
                                if self.is_running(&id) {
                                    self.stop(&id);
                                }
                            }
                            ui.close_menu();
                        }
                        if ui.button("Stop All").clicked() {
                            self.stop_all();
                            ui.close_menu();
                        }
                        ui.separator();
                        if ui.button("Delete").clicked() {
                            let ids = self.selected_ids();
                            self.remove_many(&ids);
                            ui.close_menu();
                        }
                        if ui
                            .button(RichText::new("Delete with files...").color(RED))
                            .clicked()
                        {
                            let ids = self.selected_ids();
                            self.confirm_delete_files = Some(ids);
                            ui.close_menu();
                        }
                        if ui.button("Delete completed").clicked() {
                            self.delete_completed();
                            ui.close_menu();
                        }
                    });
                    ui.menu_button("Options", |ui| {
                        if ui.button("Configuration...").clicked() {
                            self.options = Some(self.cfg.clone());
                            ui.close_menu();
                        }
                        if ui.button("Scheduler...").clicked() {
                            self.scheduler = true;
                            ui.close_menu();
                        }
                    });
                    ui.menu_button("Help", |ui| {
                        if ui.button("About TDM...").clicked() {
                            self.about = true;
                            ui.close_menu();
                        }
                    });
                    ui.menu_button("Registration", |ui| {
                        if ui.button("Register...").clicked() {
                            self.register_joke = true;
                            ui.close_menu();
                        }
                        if ui.button("Enter serial number...").clicked() {
                            self.register_joke = true;
                            ui.close_menu();
                        }
                    });
                });
            });
    }

    fn toolbar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("toolbar")
            .exact_height(66.0)
            .frame(egui::Frame::none().fill(TOOLBAR_BOTTOM))
            .show(ctx, |ui| {
                let rect = ui.max_rect();
                let painter = ui.painter();
                // smooth vertical gradient toolbar
                let steps = 12;
                for i in 0..steps {
                    let t0 = i as f32 / steps as f32;
                    let t1 = (i + 1) as f32 / steps as f32;
                    let y0 = rect.top() + rect.height() * t0;
                    let y1 = rect.top() + rect.height() * t1;
                    let c = lerp_color(TOOLBAR_TOP, TOOLBAR_BOTTOM, (t0 + t1) * 0.5);
                    painter.rect_filled(
                        egui::Rect::from_min_max(egui::pos2(rect.left(), y0), egui::pos2(rect.right(), y1)),
                        0.0,
                        c,
                    );
                }
                painter.line_segment(
                    [egui::pos2(rect.min.x, rect.max.y - 1.0), egui::pos2(rect.max.x, rect.max.y - 1.0)],
                    Stroke::new(1.0_f32, BORDER),
                );

                let selected_ids = self.selected_ids();
                let has_sel = !selected_ids.is_empty();
                let active = selected_ids.iter().any(|id| self.is_running(id));
                let resumable = selected_ids.iter().any(|id| {
                    !self.is_running(id)
                        && self
                            .records
                            .iter()
                            .find(|d| &d.id == id)
                            .map(|d| d.status != Status::Completed)
                            .unwrap_or(false)
                });

                ui.add_space(3.0);
                ui.horizontal(|ui| {
                    ui.add_space(4.0);
                    let r = tool_button(ui, "Add URL", ToolIcon::AddUrl, true, true);
                    if r.clicked() {
                        self.open_add();
                    }
                    tool_sep(ui);
                    if tool_button(ui, "Resume", ToolIcon::Resume, resumable, false).clicked() {
                        for id in self.selected_ids() {
                            if !self.is_running(&id) {
                                if let Some(dl) = self.records.iter().find(|d| d.id == id) {
                                    if dl.status != Status::Completed {
                                        self.start(id.clone());
                                    }
                                }
                            }
                        }
                        self.status_msg = "Resuming...".into();
                    }
                    if tool_button(ui, "Stop", ToolIcon::Stop, active, false).clicked() {
                        for id in self.selected_ids() {
                            if self.is_running(&id) {
                                self.stop(&id);
                            }
                        }
                    }
                    if tool_button(ui, "Stop All", ToolIcon::StopAll, true, false).clicked() {
                        self.stop_all();
                    }
                    if tool_button(ui, "Delete", ToolIcon::Delete, has_sel, false).clicked() {
                        let ids = self.selected_ids();
                        self.remove_many(&ids);
                    }
                    if tool_button(ui, "Delete Co...", ToolIcon::DeleteDone, true, false).clicked() {
                        self.delete_completed();
                    }
                    tool_sep(ui);
                    if tool_button(ui, "Options", ToolIcon::Options, true, false).clicked() {
                        self.options = Some(self.cfg.clone());
                    }
                    if tool_button(ui, "Scheduler", ToolIcon::Scheduler, true, false).clicked() {
                        self.scheduler = true;
                    }
                    tool_sep(ui);
                    if tool_button(ui, "Start Qu...", ToolIcon::StartQueue, !self.queue_running, true).clicked() {
                        self.queue_running = true;
                        let ids: Vec<String> = self
                            .records
                            .iter()
                            .filter(|d| d.status != Status::Completed && !self.is_running(&d.id))
                            .map(|d| d.id.clone())
                            .collect();
                        for id in ids {
                            self.start(id);
                        }
                        self.status_msg = "Queue started".into();
                    }
                    if tool_button(ui, "Stop Qu...", ToolIcon::StopQueue, self.queue_running, true).clicked() {
                        self.queue_running = false;
                        self.stop_all();
                    }
                    tool_sep(ui);
                    if tool_button(ui, "Tell a Friend", ToolIcon::TellFriend, true, false).clicked() {
                        self.status_msg = "Thanks for telling your friends about TDM!".into();
                    }
                });
            });
    }

    fn status_bar(&mut self, ctx: &egui::Context) {
        let selected_count = self.selected.len();
        let primary = self.primary_selected();
        egui::TopBottomPanel::bottom("status")
            .exact_height(22.0)
            .frame(egui::Frame::none().fill(BG).inner_margin(egui::Margin::symmetric(6.0, 2.0)))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    let selected = primary
                        .as_ref()
                        .and_then(|id| self.records.iter().find(|d| &d.id == id || d.short_id() == id.as_str()));
                    match selected {
                        Some(dl) => {
                            if selected_count > 1 {
                                ui.label(
                                    RichText::new(format!("{selected_count} selected"))
                                        .size(11.0)
                                        .strong()
                                        .color(BLUE),
                                );
                                ui.add_space(8.0);
                            }
                            ui.label(RichText::new(&dl.file_name).size(11.0).color(TEXT));
                            ui.add_space(10.0);
                            ui.label(RichText::new(human_bytes(dl.size)).size(11.0).color(DIM));
                        }
                        None => {
                            ui.label(RichText::new(&self.status_msg).size(11.0).color(DIM));
                        }
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if self.total_speed > 0 {
                            ui.label(
                                RichText::new(human_speed(self.total_speed))
                                    .size(11.0)
                                    .color(BLUE),
                            );
                        }
                        ui.label(
                            RichText::new(format!("{} downloads", self.records.len()))
                                .size(11.0)
                                .color(DIM),
                        );
                    });
                });
            });
    }

    fn categories(&mut self, ctx: &egui::Context) {
        egui::SidePanel::left("categories")
            .exact_width(150.0)
            .resizable(false)
            .frame(egui::Frame::none().fill(PANEL_BG).inner_margin(egui::Margin::symmetric(2.0, 2.0)))
            .show(ctx, |ui| {
                let rect = ui.max_rect();
                ui.painter().line_segment(
                    [egui::pos2(rect.max.x - 1.0, rect.min.y), egui::pos2(rect.max.x - 1.0, rect.max.y)],
                    Stroke::new(1.0_f32, BORDER),
                );
                ui.add_space(3.0);
                ui.horizontal(|ui| {
                    ui.add_space(4.0);
                    ui.label(RichText::new("Categories").size(11.5).strong().color(TEXT));
                });
                ui.add_space(3.0);

                let mut entries: Vec<(Filter, &str, CatIcon)> = vec![
                    (Filter::All, "All Downloads", CatIcon::Folder),
                    (Filter::Category("Compressed".into()), "Compressed", CatIcon::Zip),
                    (Filter::Category("Documents".into()), "Documents", CatIcon::Doc),
                    (Filter::Category("Music".into()), "Music", CatIcon::Music),
                    (Filter::Category("Programs".into()), "Programs", CatIcon::Gear),
                    (Filter::Category("Video".into()), "Video", CatIcon::Video),
                ];
                entries.push((Filter::Unfinished, "Unfinished", CatIcon::FolderOpen));
                entries.push((Filter::Finished, "Finished", CatIcon::Checked));

                for (filter, label, icon) in entries {
                    let selected = self.filter == filter;
                    let (rect, resp) = ui.allocate_exact_size(
                        egui::vec2(ui.available_width(), 20.0),
                        egui::Sense::click(),
                    );
                    let painter = ui.painter();
                    if selected {
                        painter.rect_filled(rect, Rounding::ZERO, Color32::from_rgb(214, 232, 252));
                        painter.rect_stroke(
                            rect.shrink(0.5),
                            Rounding::ZERO,
                            Stroke::new(1.0_f32, Color32::from_rgb(160, 190, 225)),
                        );
                    } else if resp.hovered() {
                        painter.rect_filled(rect, Rounding::ZERO, Color32::from_rgb(232, 240, 250));
                    }
                    draw_cat_icon(painter, egui::Rect::from_center_size(
                        egui::pos2(rect.left() + 12.0, rect.center().y),
                        egui::vec2(15.0, 15.0),
                    ), icon);
                    painter.text(
                        egui::pos2(rect.left() + 24.0, rect.center().y),
                        Align2::LEFT_CENTER,
                        label,
                        FontId::proportional(11.5),
                        TEXT,
                    );
                    if selected {
                        // fake bold, like a focused category
                        painter.text(
                            egui::pos2(rect.left() + 24.5, rect.center().y),
                            Align2::LEFT_CENTER,
                            label,
                            FontId::proportional(11.5),
                            TEXT,
                        );
                    }
                    if resp.clicked() {
                        self.filter = filter.clone();
                        self.selected.clear();
                        self.selection_anchor = None;
                    }
                }
            });
    }

    fn fit_columns(&mut self, total: f32) {
        for (i, width) in self.column_widths.iter_mut().enumerate() {
            *width = width.max(MIN_COLS[i]);
        }
        let mut fixed: f32 = self.column_widths[..7].iter().sum();
        if fixed + MIN_COLS[7] > total {
            let excess = fixed + MIN_COLS[7] - total;
            self.column_widths[0] = (self.column_widths[0] - excess).max(MIN_COLS[0]);
            fixed = self.column_widths[..7].iter().sum();
        }
        self.column_widths[7] = (total - fixed).max(MIN_COLS[7]);
    }

    fn table(&mut self, ctx: &egui::Context) {
        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(Color32::WHITE))
            .show(ctx, |ui| {
                let width = ui.available_width();
                if !self.columns_ready {
                    self.column_widths = col_widths(width);
                    self.columns_ready = true;
                }
                self.fit_columns(width);
                let widths = self.column_widths;

                // header
                let (hrect, _) = ui.allocate_exact_size(egui::vec2(width, HEADER_H), egui::Sense::hover());
                let painter = ui.painter();
                let mid = hrect.center().y;
                painter.rect_filled(
                    egui::Rect::from_min_max(hrect.min, egui::pos2(hrect.max.x, mid)),
                    0.0,
                    HEADER_TOP,
                );
                painter.rect_filled(
                    egui::Rect::from_min_max(egui::pos2(hrect.min.x, mid), hrect.max),
                    0.0,
                    HEADER_BOTTOM,
                );
                painter.line_segment(
                    [egui::pos2(hrect.min.x, hrect.min.y + 1.0), egui::pos2(hrect.max.x, hrect.min.y + 1.0)],
                    Stroke::new(1.0_f32, Color32::WHITE),
                );
                painter.line_segment(
                    [egui::pos2(hrect.min.x, hrect.max.y), egui::pos2(hrect.max.x, hrect.max.y)],
                    Stroke::new(1.0_f32, BORDER),
                );
                let labels = [
                    "File Name",
                    "",
                    "Size",
                    "Status",
                    "Time left",
                    "Transfer rate",
                    "Last Try",
                    "Description",
                ];
                let pointer = ui.input(|i| i.pointer.hover_pos());
                let primary_down = ui.input(|i| i.pointer.primary_down());
                let primary_pressed = ui.input(|i| i.pointer.primary_pressed());
                let mut hover_boundary = false;
                let mut x = hrect.left();
                for (i, label) in labels.iter().enumerate() {
                    if i > 0 {
                        painter.line_segment(
                            [egui::pos2(x, hrect.top() + 2.0), egui::pos2(x, hrect.bottom() - 2.0)],
                            Stroke::new(1.0_f32, Color32::from_rgb(178, 186, 200)),
                        );
                    }
                    if !label.is_empty() {
                        let clip = egui::Rect::from_min_max(
                            egui::pos2(x + 2.0, hrect.top()),
                            egui::pos2(x + widths[i] - 3.0, hrect.bottom()),
                        );
                        painter.with_clip_rect(clip).text(
                            egui::pos2(x + 4.0, hrect.center().y),
                            Align2::LEFT_CENTER,
                            *label,
                            FontId::proportional(11.0),
                            TEXT,
                        );
                    }
                    if i < 7 {
                        let boundary = x + widths[i];
                        let near = pointer
                            .map(|p| hrect.contains(p) && (p.x - boundary).abs() <= 4.0)
                            .unwrap_or(false);
                        if near {
                            hover_boundary = true;
                        }
                        if primary_pressed && near && self.resize_drag.is_none() {
                            self.resize_drag = Some(i);
                            self.resize_start_x = pointer.map(|p| p.x).unwrap_or(boundary);
                            self.resize_start_left = widths[i];
                            self.resize_start_next = widths[i + 1];
                            self.resize_start_desc = widths[7];
                        }
                        if self.resize_drag == Some(i) && primary_down {
                            if let Some(p) = pointer {
                                let start_pair = self.resize_start_left + self.resize_start_next;
                                let min_left = MIN_COLS[i];
                                let min_right = MIN_COLS[i + 1];
                                let desired = self.resize_start_left + p.x - self.resize_start_x;
                                if i + 1 == 7 {
                                    let left = desired.clamp(min_left, start_pair - min_right);
                                    self.column_widths[i] = left;
                                    self.column_widths[i + 1] = start_pair - left;
                                } else {
                                    let max_pair = (start_pair - min_right).max(min_left);
                                    if desired <= max_pair {
                                        let left = desired.clamp(min_left, max_pair);
                                        self.column_widths[i] = left;
                                        self.column_widths[i + 1] = start_pair - left;
                                        self.column_widths[7] = self.resize_start_desc;
                                    } else {
                                        let extra = (desired - max_pair)
                                            .min((self.resize_start_desc - 70.0).max(0.0));
                                        self.column_widths[i] = max_pair + extra;
                                        self.column_widths[i + 1] = min_right;
                                        self.column_widths[7] = self.resize_start_desc - extra;
                                    }
                                }
                            }
                        }
                        if near || self.resize_drag == Some(i) {
                            painter.line_segment(
                                [
                                    egui::pos2(boundary, hrect.top() + 3.0),
                                    egui::pos2(boundary, hrect.bottom() - 3.0),
                                ],
                                Stroke::new(1.0_f32, BLUE),
                            );
                        }
                    }
                    x += widths[i];
                }
                if self.resize_drag.is_some() && !primary_down {
                    self.cfg.column_widths = Some(self.column_widths);
                    let _ = self.cfg.save();
                    self.resize_drag = None;
                }
                if hover_boundary || self.resize_drag.is_some() {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
                }

                let items = self.visible();
                let mut clicked: Option<String> = None;
                let mut click_toggle = false;
                let mut click_range = false;
                let mut double_clicked: Option<String> = None;
                let mut context_action: Option<(u8, String)> = None;

                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        if items.is_empty() {
                            ui.add_space(40.0);
                            ui.vertical_centered(|ui| {
                                ui.label(RichText::new("No downloads in this category").size(12.0).color(DIM));
                            });
                            return;
                        }
                        for dl in &items {
                            let selected = self.selected.contains(&dl.id);
                            let done = self.done_of(dl);
                            let speed = self.speed_of(&dl.id);
                            let running = self.is_running(&dl.id);

                            let (rect, resp) = ui.allocate_exact_size(
                                egui::vec2(ui.available_width(), ROW_H),
                                egui::Sense::click(),
                            );
                            let painter = ui.painter();
                            if selected {
                                painter.rect_filled(rect, Rounding::ZERO, SELECTED);
                            } else if resp.hovered() {
                                painter.rect_filled(rect, Rounding::ZERO, Color32::from_rgb(240, 246, 254));
                            } else {
                                painter.rect_filled(rect, Rounding::ZERO, Color32::WHITE);
                            }
                            let fg = if selected { Color32::WHITE } else { TEXT };

                            let mut x = rect.left();
                            // file icon + name
                            painter.line_segment(
                                [egui::pos2(x + widths[0] - 1.0, rect.top()), egui::pos2(x + widths[0] - 1.0, rect.bottom())],
                                Stroke::new(1.0_f32, GRID),
                            );
                            draw_file_icon(painter, egui::Rect::from_center_size(
                                egui::pos2(x + 10.0, rect.center().y),
                                egui::vec2(14.0, 16.0),
                            ), &dl.file_name);
                            let name_clip = egui::Rect::from_min_max(
                                egui::pos2(x + 20.0, rect.top()),
                                egui::pos2(x + widths[0] - 4.0, rect.bottom()),
                            );
                            let name_chars = ((widths[0] - 26.0) / 6.1).max(8.0) as usize;
                            painter.with_clip_rect(name_clip).text(
                                egui::pos2(x + 20.0, rect.center().y),
                                Align2::LEFT_CENTER,
                                ellipsize(&dl.file_name, name_chars),
                                FontId::proportional(11.5),
                                fg,
                            );
                            x += widths[0];

                            // Q (status icon)
                            let qrect = egui::Rect::from_center_size(
                                egui::pos2(x + widths[1] / 2.0, rect.center().y),
                                egui::vec2(14.0, 14.0),
                            );
                            draw_status_icon(painter, qrect, dl.status, running);
                            x += widths[1];

                            let mut cell = |idx: usize, text: &str, color: Color32| {
                                let clip = egui::Rect::from_min_max(
                                    egui::pos2(x + 2.0, rect.top()),
                                    egui::pos2(x + widths[idx] - 4.0, rect.bottom()),
                                );
                                painter.with_clip_rect(clip).text(
                                    egui::pos2(x + 4.0, rect.center().y),
                                    Align2::LEFT_CENTER,
                                    text,
                                    FontId::proportional(11.5),
                                    if selected { Color32::WHITE } else { color },
                                );
                                x += widths[idx];
                            };
                            cell(2, &human_bytes(dl.size), TEXT);
                            let pct = if dl.size > 0 {
                                (done as f64 / dl.size as f64 * 100.0).min(100.0)
                            } else {
                                0.0
                            };
                            let (st, stc) = if dl.size == 0
                                && matches!(dl.status, Status::Downloading | Status::Connecting)
                            {
                                ("downloading".to_string(), BLUE)
                            } else {
                                row_status(dl.status, running, pct)
                            };
                            cell(3, &st, stc);
                            let eta = if speed > 0 && dl.size > done {
                                human_duration((dl.size - done) / speed)
                            } else {
                                String::new()
                            };
                            cell(4, &eta, TEXT);
                            let rate = if speed > 0 { human_speed(speed) } else { String::new() };
                            cell(5, &rate, BLUE);
                            let date = format_date(dl.updated_at.max(dl.created_at) as u64);
                            cell(6, &date, TEXT);
                            let _ = cell(7, "", TEXT);

                            // grid lines
                            for i in 1..8 {
                                let gx = rect.left() + widths[..i].iter().sum::<f32>();
                                painter.line_segment(
                                    [egui::pos2(gx, rect.top()), egui::pos2(gx, rect.bottom())],
                                    Stroke::new(1.0_f32, if selected { Color32::from_rgb(90, 140, 210) } else { GRID }),
                                );
                            }
                            painter.line_segment(
                                [egui::pos2(rect.left(), rect.bottom()), egui::pos2(rect.right(), rect.bottom())],
                                Stroke::new(1.0_f32, GRID),
                            );

                            if resp.clicked() {
                                clicked = Some(dl.id.clone());
                                let mods = ui.input(|i| i.modifiers);
                                click_toggle = mods.ctrl || mods.command;
                                click_range = mods.shift;
                            }
                            if resp.double_clicked() {
                                double_clicked = Some(dl.id.clone());
                            }

                            let row_selected = self.selected.contains(&dl.id);
                            let menu_ids: Vec<String> = if row_selected {
                                self.selected_ids()
                            } else {
                                vec![dl.id.clone()]
                            };
                            let menu_multi = menu_ids.len() > 1;
                            let menu_running = menu_ids.iter().any(|id| self.is_running(id));
                            let menu_resumable = menu_ids.iter().any(|id| {
                                !self.is_running(id)
                                    && self
                                        .records
                                        .iter()
                                        .find(|d| &d.id == id)
                                        .map(|d| d.status != Status::Completed)
                                        .unwrap_or(false)
                            });
                            let menu_urls: Vec<String> = menu_ids
                                .iter()
                                .filter_map(|id| {
                                    self.records
                                        .iter()
                                        .find(|d| &d.id == id)
                                        .map(|d| d.url.clone())
                                })
                                .collect();
                            let path = dl.final_path();
                            let path_exists = dl.status == Status::Completed && path.exists();
                            resp.context_menu(|ui| {
                                ui.set_min_width(if menu_multi { 220.0 } else { 175.0 });
                                if ui.button("Details").clicked() {
                                    context_action = Some((0, dl.id.clone()));
                                    ui.close_menu();
                                }
                                if path_exists && ui.button("Open file").clicked() {
                                    context_action = Some((1, dl.id.clone()));
                                    ui.close_menu();
                                }
                                if ui.button("Open folder").clicked() {
                                    context_action = Some((2, dl.id.clone()));
                                    ui.close_menu();
                                }
                                ui.separator();
                                if menu_running {
                                    let label = if menu_multi { "Pause selected" } else { "Pause" };
                                    if ui.button(label).clicked() {
                                        context_action = Some((3, dl.id.clone()));
                                        ui.close_menu();
                                    }
                                }
                                if menu_resumable {
                                    let label = if menu_multi { "Resume selected" } else { "Resume" };
                                    if ui.button(label).clicked() {
                                        context_action = Some((4, dl.id.clone()));
                                        ui.close_menu();
                                    }
                                }
                                let copy_label = if menu_multi { "Copy URLs" } else { "Copy URL" };
                                if ui.button(copy_label).clicked() {
                                    ui.output_mut(|o| o.copied_text = menu_urls.join("\n"));
                                    ui.close_menu();
                                }
                                ui.separator();
                                if !row_selected && ui.button("Select all").clicked() {
                                    context_action = Some((6, dl.id.clone()));
                                    ui.close_menu();
                                }
                                let remove_label = if menu_multi {
                                    "Remove selected from list"
                                } else {
                                    "Remove from list"
                                };
                                if ui.button(remove_label).clicked() {
                                    context_action = Some((5, dl.id.clone()));
                                    ui.close_menu();
                                }
                                let delete_label = if menu_multi {
                                    "Remove selected and delete files"
                                } else {
                                    "Remove from list and delete file"
                                };
                                if ui
                                    .button(RichText::new(delete_label).color(RED))
                                    .clicked()
                                {
                                    context_action = Some((7, dl.id.clone()));
                                    ui.close_menu();
                                }
                            });
                        }
                    });

                if let Some((action, id)) = context_action {
                    let was_selected = self.selected.contains(&id);
                    if !was_selected && action != 6 {
                        self.select_only(&id);
                    }
                    match action {
                        0 => {
                            self.details = Some(id);
                        }
                        1 => {
                            if let Some(dl) = self.store.resolve(&id) {
                                open_file(&dl.final_path());
                            }
                        }
                        2 => {
                            if let Some(dl) = self.store.resolve(&id) {
                                open_folder(std::path::Path::new(&dl.dir));
                            }
                        }
                        3 => {
                            for selected in self.selected_ids() {
                                if self.is_running(&selected) {
                                    self.stop(&selected);
                                }
                            }
                        }
                        4 => {
                            for selected in self.selected_ids() {
                                if !self.is_running(&selected) {
                                    if let Some(dl) = self
                                        .records
                                        .iter()
                                        .find(|d| d.id == selected)
                                    {
                                        if dl.status != Status::Completed {
                                            self.start(selected.clone());
                                        }
                                    }
                                }
                            }
                        }
                        5 => {
                            let ids = self.selected_ids();
                            self.remove_many(&ids);
                        }
                        6 => self.select_all_visible(),
                        7 => {
                            let ids = self.selected_ids();
                            self.confirm_delete_files = Some(ids);
                        }
                        _ => {}
                    }
                }

                if let Some(id) = clicked {
                    if click_range {
                        self.select_range(&id);
                    } else if click_toggle {
                        self.toggle_select(&id);
                    } else {
                        self.select_only(&id);
                    }
                }
                if let Some(id) = double_clicked {
                    self.select_only(&id.clone());
                    self.details = Some(id);
                }
            });
    }

    fn about_dialog(&mut self, ctx: &egui::Context) {
        if self.flag_texture.is_none() {
            self.flag_texture = load_flag_texture(ctx);
        }
        if self.logo_texture.is_none() {
            self.logo_texture = load_logo_texture(ctx);
        }
        let flag = self.flag_texture.clone();
        let logo = self.logo_texture.clone();
        let mut close = false;
        egui::Window::new("About TDM")
            .collapsible(false)
            .resizable(false)
            .default_width(430.0)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, -40.0))
            .show(ctx, |ui| {
                ui.add_space(8.0);
                ui.vertical_centered(|ui| {
                    if let Some(tex) = &logo {
                        let (lrect, _) = ui.allocate_exact_size(egui::vec2(88.0, 88.0), egui::Sense::hover());
                        ui.painter().image(
                            tex.id(),
                            lrect,
                            egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                            Color32::WHITE,
                        );
                        ui.add_space(6.0);
                    }
                    ui.label(RichText::new("TDM").size(28.0).strong().color(BLUE));
                    ui.label(RichText::new("The Download Manager").size(12.5).color(DIM));
                    ui.label(
                        RichText::new(format!("Version {}", env!("CARGO_PKG_VERSION")))
                            .size(10.5)
                            .color(DIM),
                    );
                    ui.add_space(10.0);
                    ui.label(
                        RichText::new("Free and open source software")
                            .size(12.5)
                            .strong(),
                    );
                    ui.add_space(4.0);
                    ui.label(
                        RichText::new(
                            "TDM is released under the MIT License. You may use, study, \
                             modify and redistribute it.",
                        )
                        .size(11.0)
                        .color(DIM),
                    );
                    ui.add_space(6.0);
                    ui.hyperlink_to("Source code on GitHub", PROJECT_URL);
                    ui.hyperlink_to("MIT License", LICENSE_URL);
                    ui.hyperlink_to("Report an issue", ISSUES_URL);
                    ui.hyperlink_to("Third-party notices", NOTICE_URL);
                    ui.add_space(8.0);
                    ui.label(
                        RichText::new(
                            "Built with Rust + egui. Video muxing uses FFmpeg, a separate \
                             project with its own license.",
                        )
                        .size(10.5)
                        .color(DIM),
                    );
                    ui.label(
                        RichText::new("No ads · no telemetry · no registration")
                            .size(10.5)
                            .color(DIM),
                    );
                    ui.add_space(8.0);
                    ui.label(
                        RichText::new("Current developers")
                            .size(10.5)
                            .strong()
                            .color(DIM),
                    );
                    ui.label(RichText::new("G. Thilina Dinesh Perera").size(11.0));
                    ui.add_space(8.0);
                    about_footer(ui, flag.as_ref());
                    ui.add_space(2.0);
                    ui.label(
                        RichText::new("MIT License · see LICENSE")
                            .size(9.5)
                            .color(DIM),
                    );
                    ui.label(
                        RichText::new("SPDX-License-Identifier: MIT")
                            .size(9.0)
                            .color(DIM),
                    );
                });
                ui.add_space(14.0);
                ui.separator();
                ui.add_space(6.0);
                ui.vertical_centered(|ui| {
                    if ui.button("Close").clicked() {
                        close = true;
                    }
                });
            });
        if close {
            self.about = false;
        }
    }

    fn registration_dialog(&mut self, ctx: &egui::Context) {
        let mut close = false;
        egui::Window::new("Registration")
            .collapsible(false)
            .resizable(false)
            .default_width(440.0)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, -30.0))
            .show(ctx, |ui| {
                ui.add_space(8.0);
                ui.vertical_centered(|ui| {
                    ui.label(RichText::new("Gotcha!").size(26.0).strong().color(RED));
                    ui.add_space(6.0);
                    ui.label(
                        RichText::new("There is no registration, and there never was.")
                            .size(12.5),
                    );
                    ui.add_space(8.0);
                    ui.label(
                        RichText::new(
                            "You just found the most honest menu in the whole application. \
                             TDM is 100% free and open source — no serial numbers, no trials, \
                             no \"buy now\" popups, no 30-day countdown, and absolutely no \
                             phone-home.",
                        )
                        .size(11.5),
                    );
                    ui.add_space(8.0);
                    ui.label(
                        RichText::new("Nice try though. We admire the curiosity.")
                            .size(11.5)
                            .italics()
                            .color(DIM),
                    );
                    ui.add_space(14.0);
                    if ui.button("OK, I believe you — keep it free").clicked() {
                        close = true;
                    }
                });
            });
        if close {
            self.register_joke = false;
        }
    }

    fn add_dialog_default(&self) -> AddDialog {
        AddDialog {
            url: String::new(),
            dir: self.cfg.download_dir.clone(),
            name: String::new(),
            connections: self.cfg.connections,
            error: None,
            focus_url: true,
        }
    }

    fn open_add(&mut self) {
        self.add = Some(self.add_dialog_default());
    }

    fn scheduler_dialog(&mut self, ctx: &egui::Context) {
        let mut close = false;
        egui::Window::new("Scheduler")
            .collapsible(false)
            .resizable(false)
            .default_width(360.0)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, -30.0))
            .show(ctx, |ui| {
                ui.add_space(6.0);
                ui.label("Scheduled downloads are not configured yet.");
                ui.label(RichText::new("Time-based scheduling is not configured yet.").size(11.0).color(DIM));
                ui.add_space(8.0);
                if ui.button("Close").clicked() {
                    close = true;
                }
            });
        if close {
            self.scheduler = false;
        }
    }

    fn add_dialog(&mut self, ctx: &egui::Context) {
        let mut action: Option<bool> = None;
        let mut close = false;
        let dialog = self.add.as_mut().unwrap();

        egui::Window::new("Add URL")
            .collapsible(false)
            .resizable(false)
            .default_width(520.0)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, -40.0))
            .show(ctx, |ui| {
                ui.add_space(4.0);
                ui.label("Address:");
                let field_width = ui.available_width().max(120.0);
                let url_edit = ui.add(
                    egui::TextEdit::singleline(&mut dialog.url)
                        .hint_text("https://example.com/file.zip")
                        .desired_width(field_width),
                );
                if dialog.focus_url {
                    url_edit.request_focus();
                    dialog.focus_url = false;
                }
                if ui.input(|i| i.key_pressed(egui::Key::Enter)) && !dialog.url.trim().is_empty() {
                    action = Some(true);
                }
                ui.add_space(6.0);
                ui.label("Save to:");
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut dialog.dir).desired_width(360.0));
                });
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.label("File name:");
                    ui.add(
                        egui::TextEdit::singleline(&mut dialog.name)
                            .hint_text("(auto from URL)")
                            .desired_width(250.0),
                    );
                });
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.label("Connections:");
                    ui.add(egui::Slider::new(&mut dialog.connections, 1..=32));
                });
                if let Some(err) = &dialog.error {
                    ui.add_space(4.0);
                    ui.colored_label(RED, err);
                }
                ui.add_space(10.0);
                ui.separator();
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.add(egui::Button::new(RichText::new("Start Download").strong())).clicked() {
                        action = Some(true);
                    }
                    if ui.button("Download Later").clicked() {
                        action = Some(false);
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.button("Cancel").clicked() {
                            close = true;
                        }
                    });
                });
            });

        if close {
            self.add = None;
            return;
        }
        if let Some(start_now) = action {
            self.add_download(start_now);
        }
    }

    fn options_dialog(&mut self, ctx: &egui::Context) {
        let mut save = false;
        let mut close = false;
        let mut startup = self.startup;
        let cfg = self.options.as_mut().unwrap();
        let mut speed_text = if cfg.speed_limit == 0 {
            "0".to_string()
        } else {
            human_bytes(cfg.speed_limit).replace(' ', "")
        };

        egui::Window::new("TDM Configuration")
            .collapsible(false)
            .resizable(false)
            .default_width(460.0)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, -20.0))
            .show(ctx, |ui| {
                ui.add_space(4.0);
                ui.label("Default download folder:");
                let field_width = ui.available_width().max(120.0);
                ui.add(egui::TextEdit::singleline(&mut cfg.download_dir).desired_width(field_width));
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.label("Connections per download:");
                    ui.add(egui::Slider::new(&mut cfg.connections, 1..=32));
                });
                ui.horizontal(|ui| {
                    ui.label("Simultaneous downloads:");
                    ui.add(egui::Slider::new(&mut cfg.max_concurrent, 1..=16));
                });
                ui.horizontal(|ui| {
                    ui.label("Speed limit (0 = unlimited):");
                    ui.add(egui::TextEdit::singleline(&mut speed_text).desired_width(90.0));
                });
                ui.add_space(4.0);
                ui.checkbox(&mut cfg.categories, "Save into category folders (Video, Music, ...)");
                ui.checkbox(
                    &mut startup,
                    "Start TDM with Windows (minimized to the tray)",
                );
                ui.add_space(8.0);
                ui.label("Proxy (e.g. http://127.0.0.1:8080):");
                let mut proxy = cfg.proxy.clone().unwrap_or_default();
                let proxy_width = ui.available_width().max(120.0);
                if ui
                    .add(egui::TextEdit::singleline(&mut proxy).desired_width(proxy_width))
                    .changed()
                {
                    cfg.proxy = if proxy.trim().is_empty() {
                        None
                    } else {
                        Some(proxy.trim().to_string())
                    };
                }
                ui.add_space(10.0);
                ui.separator();
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.button("Save").clicked() {
                        save = true;
                    }
                    if ui.button("Cancel").clicked() {
                        close = true;
                    }
                });
            });

        if save {
            cfg.speed_limit = parse_size(&speed_text).unwrap_or(0);
            self.cfg = cfg.clone();
            let _ = self.cfg.save();
            if startup != self.startup {
                set_startup(startup);
                self.startup = startup;
            }
            self.status_msg = "Configuration saved".into();
            self.options = None;
            return;
        }
        if close {
            self.options = None;
        }
    }

    fn confirm_delete_files_dialog(&mut self, ctx: &egui::Context) {
        let Some(ids) = self.confirm_delete_files.clone() else {
            return;
        };
        let mut confirm = false;
        let mut close = false;
        let names: Vec<String> = ids
            .iter()
            .filter_map(|id| self.store.resolve(id).map(|dl| dl.file_name))
            .collect();

        egui::Window::new("Delete files")
            .collapsible(false)
            .resizable(false)
            .default_width(430.0)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, -10.0))
            .show(ctx, |ui| {
                ui.add_space(4.0);
                ui.label(
                    RichText::new(if names.len() == 1 {
                        "Remove this download from the list and delete its file?"
                            .to_string()
                    } else {
                        format!(
                            "Remove these {} downloads from the list and delete their files?",
                            names.len()
                        )
                    })
                    .strong()
                    .size(13.0),
                );
                ui.add_space(6.0);
                egui::Frame::none()
                    .fill(Color32::from_rgb(250, 250, 252))
                    .stroke(Stroke::new(1.0, GRID))
                    .rounding(Rounding::same(3.0))
                    .inner_margin(egui::Margin::same(6.0))
                    .show(ui, |ui| {
                        egui::ScrollArea::vertical()
                            .max_height(120.0)
                            .auto_shrink([false, true])
                            .show(ui, |ui| {
                                for name in &names {
                                    ui.label(
                                        RichText::new(ellipsize(name, 62))
                                            .size(11.0)
                                            .color(TEXT),
                                    );
                                }
                            });
                    });
                ui.add_space(6.0);
                ui.colored_label(
                    RED,
                    "The downloaded file and any partial parts will be deleted from disk.",
                );
                ui.label(
                    RichText::new("This cannot be undone.")
                        .size(10.5)
                        .color(DIM),
                );
                ui.add_space(6.0);
                ui.separator();
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    let delete = egui::Button::new(
                        RichText::new("Delete files")
                            .size(11.5)
                            .strong()
                            .color(Color32::WHITE),
                    )
                    .fill(RED)
                    .rounding(Rounding::same(3.0));
                    if ui.add(delete).clicked() {
                        confirm = true;
                    }
                    if ui.button("Cancel").clicked() {
                        close = true;
                    }
                });
            });

        if confirm {
            self.delete_files_and_records(&ids);
            self.confirm_delete_files = None;
        } else if close {
            self.confirm_delete_files = None;
        }
    }

    fn details_window(&mut self, ctx: &egui::Context) {
        let Some(id) = self.details.clone() else { return };
        let dl = self
            .records
            .iter()
            .find(|d| d.id == id || d.short_id() == id)
            .cloned();
        let Some(dl) = dl else {
            self.details = None;
            return;
        };
        let mut open = true;
        let running = self.is_running(&dl.id);
        let done = self.done_of(&dl);
        let speed = self.speed_of(&dl.id);
        let frac = if dl.size > 0 {
            (done as f64 / dl.size as f64).clamp(0.0, 1.0) as f32
        } else {
            0.0
        };
        let pct = frac * 100.0;
        let eta = if speed > 0 && dl.size > done {
            (dl.size - done) / speed
        } else {
            0
        };
        let (status_text, status_color_value) = row_status(dl.status, running, pct as f64);

        egui::Window::new("Download progress")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(570.0)
            .show(ctx, |ui| {
                ui.set_min_width(545.0);
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(ellipsize(&dl.file_name, 58))
                            .size(14.0)
                            .strong()
                            .color(TEXT),
                    );
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.label(
                            RichText::new(&status_text)
                                .size(11.0)
                                .strong()
                                .color(status_color_value),
                        );
                    });
                });
                ui.label(
                    RichText::new(ellipsize(&dl.url, 92))
                        .size(10.5)
                        .color(DIM),
                );
                ui.add_space(8.0);

                egui::Frame::none()
                    .fill(Color32::from_rgb(249, 251, 254))
                    .stroke(Stroke::new(1.0, GRID))
                    .rounding(Rounding::same(4.0))
                    .inner_margin(egui::Margin::same(10.0))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new("Overall progress")
                                    .size(11.0)
                                    .strong()
                                    .color(BLUE),
                            );
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                ui.label(
                                    RichText::new(format!("{pct:.1}%"))
                                        .size(23.0)
                                        .strong()
                                        .color(BLUE),
                                );
                            });
                        });
                        ui.add_space(2.0);
                        let label = format!(
                            "{} of {}   ·   {}",
                            human_bytes(done),
                            human_bytes(dl.size),
                            human_speed(speed)
                        );
                        draw_rainbow_bar(ui, frac, false, false, 0.0, &label);
                        ui.add_space(3.0);
                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new(if speed > 0 {
                                    format!("Speed  {}", human_speed(speed))
                                } else {
                                    "Speed  —".to_string()
                                })
                                .size(11.0)
                                .color(TEXT),
                            );
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                ui.label(
                                    RichText::new(if eta > 0 {
                                        format!("Time left  {}", human_duration(eta))
                                    } else {
                                        "Time left  —".to_string()
                                    })
                                    .size(11.0)
                                    .color(DIM),
                                );
                            });
                        });
                    });

                if !dl.segments.is_empty() {
                    ui.add_space(10.0);
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new(format!("Connections ({})", dl.segments.len()))
                                .size(11.5)
                                .strong()
                                .color(TEXT),
                        );
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            ui.label(
                                RichText::new("byte range")
                                    .size(9.5)
                                    .color(DIM),
                            );
                        });
                    });
                    ui.add_space(2.0);
                    egui::Frame::none()
                        .fill(Color32::WHITE)
                        .stroke(Stroke::new(1.0, GRID))
                        .rounding(Rounding::same(3.0))
                        .show(ui, |ui| {
                            egui::ScrollArea::vertical()
                                .max_height(300.0)
                                .auto_shrink([false, true])
                                .show(ui, |ui| {
                                    for (index, segment) in dl.segments.iter().enumerate() {
                                        draw_connection_row(ui, index, segment);
                                    }
                                });
                        });
                }

                ui.add_space(10.0);
                ui.separator();
                ui.add_space(5.0);
                ui.horizontal(|ui| {
                    let primary = |text: &str| {
                        egui::Button::new(
                            RichText::new(text)
                                .size(11.5)
                                .strong()
                                .color(Color32::WHITE),
                        )
                        .fill(BLUE)
                        .rounding(Rounding::same(3.0))
                    };
                    if running {
                        if ui.add(primary("Pause")).clicked() {
                            self.stop(&dl.id);
                        }
                    } else if dl.status != Status::Completed
                        && ui.add(primary("Resume")).clicked()
                    {
                        self.start(dl.id.clone());
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.button("Close").clicked() {
                            self.details = None;
                        }
                    });
                });
            });
        if !open {
            self.details = None;
        }
    }
}

// ---------------------------------------------------------------------------
// icons and painting helpers
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum ToolIcon {
    AddUrl,
    Resume,
    Stop,
    StopAll,
    Delete,
    DeleteDone,
    Options,
    Scheduler,
    StartQueue,
    StopQueue,
    TellFriend,
}

#[derive(Clone, Copy)]
enum CatIcon {
    Folder,
    FolderOpen,
    Zip,
    Doc,
    Music,
    Gear,
    Video,
    Checked,
}

fn load_logo_texture(ctx: &egui::Context) -> Option<egui::TextureHandle> {
    let bytes = include_bytes!("../assets/tdm-256.png");
    let img = image::load_from_memory(bytes).ok()?.to_rgba8();
    let size = [img.width() as usize, img.height() as usize];
    let color = egui::ColorImage::from_rgba_unmultiplied(size, img.as_raw());
    Some(ctx.load_texture("tdm-logo", color, egui::TextureOptions::LINEAR))
}

fn app_icon_data() -> Option<egui::IconData> {
    let bytes = include_bytes!("../assets/tdm-256.png");
    let img = image::load_from_memory(bytes).ok()?.to_rgba8();
    let (width, height) = img.dimensions();
    Some(egui::IconData {
        rgba: img.into_raw(),
        width,
        height,
    })
}

fn about_footer(ui: &mut egui::Ui, flag: Option<&egui::TextureHandle>) {
    let font = FontId::proportional(11.0);
    let color = DIM;
    let left = "Built with ❤ in Sri Lanka";
    let right = "· TDM — The Download Manager";
    let left_galley = ui.fonts(|f| f.layout_no_wrap(left.to_owned(), font.clone(), color));
    let right_galley = ui.fonts(|f| f.layout_no_wrap(right.to_owned(), font.clone(), color));

    let gap = 4.0;
    let flag_w = 18.0;
    let flag_h = 9.0;
    let total = left_galley.size().x + gap + flag_w + gap + right_galley.size().x;
    let (rect, _) = ui.allocate_exact_size(egui::vec2(total, 14.0), egui::Sense::hover());
    let painter = ui.painter();
    let cy = rect.center().y;

    let mut x = rect.left();
    painter.galley(
        egui::pos2(x, cy - left_galley.size().y / 2.0),
        left_galley.clone(),
        color,
    );
    x += left_galley.size().x + gap;

    if let Some(tex) = flag {
        let fr = egui::Rect::from_min_size(egui::pos2(x, cy - flag_h / 2.0), egui::vec2(flag_w, flag_h));
        painter.image(
            tex.id(),
            fr,
            egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
            Color32::WHITE,
        );
        painter.rect_stroke(
            fr,
            Rounding::ZERO,
            Stroke::new(0.6_f32, Color32::from_rgb(120, 120, 120)),
        );
    }
    x += flag_w + gap;

    painter.galley(
        egui::pos2(x, cy - right_galley.size().y / 2.0),
        right_galley.clone(),
        color,
    );
}

fn load_flag_texture(ctx: &egui::Context) -> Option<egui::TextureHandle> {
    let bytes = include_bytes!("../assets/lk.png");
    let img = image::load_from_memory(bytes).ok()?.to_rgba8();
    let size = [img.width() as usize, img.height() as usize];
    let color = egui::ColorImage::from_rgba_unmultiplied(size, img.as_raw());
    Some(ctx.load_texture("lk-flag", color, egui::TextureOptions::LINEAR))
}

fn lerp_color(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let mix = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t) as u8;
    Color32::from_rgb(mix(a.r(), b.r()), mix(a.g(), b.g()), mix(a.b(), b.b()))
}

fn tool_button(ui: &mut egui::Ui, label: &str, icon: ToolIcon, enabled: bool, dropdown: bool) -> egui::Response {
    let size = egui::vec2(50.0, 58.0);
    let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click());
    let hovered = resp.hovered() && enabled;
    let painter = ui.painter();
    if hovered {
        painter.rect_filled(rect.shrink(1.0), Rounding::same(3.0_f32), Color32::from_rgb(255, 255, 255));
        painter.rect_stroke(rect.shrink(1.0), Rounding::same(3.0_f32), Stroke::new(1.0_f32, Color32::from_rgb(160, 180, 205)));
    }
    let icon_rect = egui::Rect::from_center_size(
        egui::pos2(rect.center().x, rect.top() + 16.0),
        egui::vec2(30.0, 30.0),
    );
    draw_tool_icon(painter, icon_rect, icon, enabled);
    painter.text(
        egui::pos2(rect.center().x, rect.bottom() - 9.0),
        Align2::CENTER_CENTER,
        label,
        FontId::proportional(9.5),
        if enabled { TEXT } else { Color32::from_rgb(150, 150, 150) },
    );
    if dropdown {
        let p = egui::pos2(icon_rect.right() - 1.0, icon_rect.bottom() - 2.0);
        painter.add(egui::Shape::convex_polygon(
            vec![p, egui::pos2(p.x - 6.0, p.y), egui::pos2(p.x - 3.0, p.y + 4.0)],
            Color32::from_rgb(60, 60, 60),
            Stroke::NONE,
        ));
    }
    resp
}

fn tool_sep(ui: &mut egui::Ui) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(9.0, 58.0), egui::Sense::hover());
    ui.painter().line_segment(
        [egui::pos2(rect.center().x, rect.top() + 8.0), egui::pos2(rect.center().x, rect.bottom() - 8.0)],
        Stroke::new(1.0_f32, Color32::from_rgb(200, 205, 215)),
    );
}

fn draw_tool_icon(painter: &egui::Painter, rect: egui::Rect, icon: ToolIcon, enabled: bool) {
    let alpha = if enabled { 255 } else { 110 };
    let c = |r: u8, g: u8, b: u8| Color32::from_rgba_unmultiplied(r, g, b, alpha);
    match icon {
        ToolIcon::AddUrl => {
            // page with a green plus
            let page = rect.shrink(3.0);
            painter.rect_filled(page, Rounding::same(2.0_f32), c(255, 255, 255));
            painter.rect_stroke(page, Rounding::same(2.0_f32), Stroke::new(1.0_f32, c(90, 110, 140)));
            let fold = 7.0;
            painter.add(egui::Shape::convex_polygon(
                vec![
                    egui::pos2(page.right() - fold, page.top()),
                    egui::pos2(page.right(), page.top() + fold),
                    egui::pos2(page.right() - fold, page.top() + fold),
                ],
                c(222, 230, 240),
                Stroke::new(0.8_f32, c(90, 110, 140)),
            ));
            for i in 0..3 {
                let y = page.top() + 10.0 + i as f32 * 4.0;
                painter.line_segment(
                    [egui::pos2(page.left() + 4.0, y), egui::pos2(page.right() - 9.0, y)],
                    Stroke::new(0.9_f32, c(150, 165, 185)),
                );
            }
            let plus = egui::pos2(rect.right() - 8.0, rect.bottom() - 8.0);
            painter.circle_filled(plus, 8.0, c(0, 150, 0));
            painter.line_segment([egui::pos2(plus.x - 4.0, plus.y), egui::pos2(plus.x + 4.0, plus.y)], Stroke::new(2.0_f32, Color32::WHITE));
            painter.line_segment([egui::pos2(plus.x, plus.y - 4.0), egui::pos2(plus.x, plus.y + 4.0)], Stroke::new(2.0_f32, Color32::WHITE));
        }
        ToolIcon::Resume => {
            painter.circle_filled(rect.center(), 13.0, c(0, 110, 200));
            painter.add(egui::Shape::convex_polygon(
                vec![
                    egui::pos2(rect.center().x - 5.0, rect.center().y - 7.0),
                    egui::pos2(rect.center().x + 8.0, rect.center().y),
                    egui::pos2(rect.center().x - 5.0, rect.center().y + 7.0),
                ],
                c(255, 255, 255),
                Stroke::NONE,
            ));
        }
        ToolIcon::Stop => {
            painter.circle_filled(rect.center(), 13.0, c(200, 30, 30));
            let r = egui::Rect::from_center_size(rect.center(), egui::vec2(14.0, 14.0));
            painter.rect_stroke(r, Rounding::ZERO, Stroke::new(3.0_f32, Color32::WHITE));
        }
        ToolIcon::StopAll => {
            for (dx, dy) in [(-5.0, -5.0), (5.0, 5.0)] {
                let r = egui::Rect::from_center_size(rect.center() + egui::vec2(dx, dy), egui::vec2(16.0, 16.0));
                painter.rect_filled(r, Rounding::same(2.0_f32), c(200, 30, 30));
                painter.line_segment([r.left_top(), r.right_bottom()], Stroke::new(2.0_f32, Color32::WHITE));
                painter.line_segment([r.right_top(), r.left_bottom()], Stroke::new(2.0_f32, Color32::WHITE));
            }
        }
        ToolIcon::Delete | ToolIcon::DeleteDone => {
            let body = egui::Rect::from_center_size(rect.center() + egui::vec2(0.0, 3.0), egui::vec2(15.0, 18.0));
            painter.rect_filled(body, Rounding::same(2.0_f32), c(170, 180, 195));
            painter.rect_stroke(body, Rounding::same(2.0_f32), Stroke::new(1.0_f32, c(90, 100, 115)));
            painter.rect_filled(
                egui::Rect::from_min_size(egui::pos2(body.left() - 3.0, body.top() - 4.0), egui::vec2(21.0, 3.0)),
                Rounding::ZERO,
                c(120, 130, 145),
            );
            for dx in [-4.0, 0.0, 4.0] {
                painter.line_segment(
                    [egui::pos2(body.center().x + dx, body.top() + 4.0), egui::pos2(body.center().x + dx, body.bottom() - 4.0)],
                    Stroke::new(1.0_f32, c(90, 100, 115)),
                );
            }
            if matches!(icon, ToolIcon::DeleteDone) {
                painter.circle_filled(egui::pos2(rect.right() - 8.0, rect.bottom() - 9.0), 7.0, c(0, 150, 0));
                painter.line_segment(
                    [egui::pos2(rect.right() - 11.0, rect.bottom() - 9.0), egui::pos2(rect.right() - 9.0, rect.bottom() - 7.0)],
                    Stroke::new(1.6_f32, Color32::WHITE),
                );
                painter.line_segment(
                    [egui::pos2(rect.right() - 9.0, rect.bottom() - 7.0), egui::pos2(rect.right() - 5.0, rect.bottom() - 12.0)],
                    Stroke::new(1.6_f32, Color32::WHITE),
                );
            }
        }
        ToolIcon::Options => {
            let center = rect.center();
            for i in 0..8 {
                let a = i as f32 * std::f32::consts::PI / 4.0;
                painter.circle_filled(center + egui::vec2(a.cos(), a.sin()) * 10.0, 3.4, c(90, 100, 115));
            }
            painter.circle_filled(center, 9.5, c(120, 130, 145));
            painter.circle_filled(center, 4.0, Color32::WHITE);
        }
        ToolIcon::Scheduler => {
            painter.circle_filled(rect.center(), 12.0, c(245, 200, 60));
            painter.circle_stroke(rect.center(), 12.0, Stroke::new(1.5_f32, c(140, 100, 0)));
            painter.line_segment([rect.center(), rect.center() + egui::vec2(0.0, -7.0)], Stroke::new(2.0_f32, c(40, 40, 40)));
            painter.line_segment([rect.center(), rect.center() + egui::vec2(5.0, 2.0)], Stroke::new(2.0_f32, c(40, 40, 40)));
        }
        ToolIcon::StartQueue => {
            painter.circle_filled(rect.center() + egui::vec2(-4.0, 0.0), 11.0, c(0, 150, 0));
            painter.add(egui::Shape::convex_polygon(
                vec![
                    rect.center() + egui::vec2(-8.0, -6.0),
                    rect.center() + egui::vec2(2.0, 0.0),
                    rect.center() + egui::vec2(-8.0, 6.0),
                ],
                Color32::WHITE,
                Stroke::NONE,
            ));
        }
        ToolIcon::StopQueue => {
            painter.circle_filled(rect.center() + egui::vec2(-4.0, 0.0), 11.0, c(200, 30, 30));
            painter.rect_stroke(
                egui::Rect::from_center_size(rect.center() + egui::vec2(-4.0, 0.0), egui::vec2(10.0, 10.0)),
                Rounding::ZERO,
                Stroke::new(2.5_f32, Color32::WHITE),
            );
        }
        ToolIcon::TellFriend => {
            let r = egui::Rect::from_center_size(rect.center(), egui::vec2(26.0, 18.0));
            painter.rect_filled(r, Rounding::same(2.0_f32), c(250, 250, 255));
            painter.rect_stroke(r, Rounding::same(2.0_f32), Stroke::new(1.0_f32, c(80, 90, 110)));
            painter.line_segment([r.left_top(), egui::pos2(r.center().x, r.center().y + 2.0)], Stroke::new(1.2_f32, c(80, 90, 110)));
            painter.line_segment([r.right_top(), egui::pos2(r.center().x, r.center().y + 2.0)], Stroke::new(1.2_f32, c(80, 90, 110)));
        }
    }
}

fn draw_cat_icon(painter: &egui::Painter, rect: egui::Rect, icon: CatIcon) {
    let folder = |r: egui::Rect, fill: Color32| {
        let tab = egui::Rect::from_min_size(r.min + egui::vec2(0.0, -2.0), egui::vec2(r.width() * 0.45, 4.0));
        painter.rect_filled(tab, Rounding::same(1.0_f32), fill);
        painter.rect_filled(r, Rounding::same(1.0_f32), fill);
    };
    match icon {
        CatIcon::Folder => folder(rect.shrink(1.0), Color32::from_rgb(240, 200, 90)),
        CatIcon::FolderOpen => {
            folder(rect.shrink(1.0).translate(egui::vec2(0.0, -1.0)), Color32::from_rgb(235, 195, 85));
            let open = egui::Rect::from_min_size(rect.min + egui::vec2(1.0, 5.0), egui::vec2(rect.width() - 1.0, rect.height() - 4.0));
            painter.rect_filled(open, Rounding::same(1.0_f32), Color32::from_rgb(250, 225, 140));
        }
        CatIcon::Zip => {
            painter.rect_filled(rect.shrink(1.0), Rounding::same(1.0_f32), Color32::from_rgb(246, 220, 110));
            painter.rect_stroke(rect.shrink(1.0), Rounding::same(1.0_f32), Stroke::new(1.0_f32, Color32::from_rgb(150, 118, 20)));
            // zipper teeth
            for i in 0..5 {
                let y = rect.top() + 3.0 + i as f32 * 2.4;
                let w = if i % 2 == 0 { 4.0 } else { 6.0 };
                painter.rect_filled(
                    egui::Rect::from_min_size(egui::pos2(rect.center().x - w / 2.0, y), egui::vec2(w, 1.6)),
                    Rounding::ZERO,
                    Color32::from_rgb(110, 85, 10),
                );
            }
        }
        CatIcon::Doc => {
            painter.rect_filled(rect.shrink(1.0), Rounding::same(1.0_f32), Color32::WHITE);
            painter.rect_stroke(rect.shrink(1.0), Rounding::same(1.0_f32), Stroke::new(1.0_f32, Color32::from_rgb(120, 130, 150)));
            for i in 0..4 {
                let y = rect.top() + 4.0 + i as f32 * 2.4;
                let x2 = rect.right() - if i % 2 == 0 { 3.0 } else { 5.0 };
                painter.line_segment(
                    [egui::pos2(rect.left() + 3.0, y), egui::pos2(x2, y)],
                    Stroke::new(1.0_f32, Color32::from_rgb(60, 90, 170)),
                );
            }
        }
        CatIcon::Music => {
            painter.circle_filled(rect.center() + egui::vec2(-2.0, 4.0), 3.0, Color32::from_rgb(60, 90, 200));
            painter.line_segment([rect.center() + egui::vec2(0.0, 4.0), rect.center() + egui::vec2(0.0, -6.0)], Stroke::new(1.5_f32, Color32::from_rgb(60, 90, 200)));
            painter.line_segment([rect.center() + egui::vec2(0.0, -6.0), rect.center() + egui::vec2(5.0, -4.0)], Stroke::new(1.5_f32, Color32::from_rgb(60, 90, 200)));
        }
        CatIcon::Gear => {
            painter.circle_filled(rect.center(), 6.0, Color32::from_rgb(110, 120, 140));
            painter.circle_filled(rect.center(), 2.5, Color32::WHITE);
        }
        CatIcon::Video => {
            painter.rect_filled(rect.shrink(1.0), Rounding::same(1.0_f32), Color32::from_rgb(60, 60, 70));
            for i in 0..3 {
                let y = rect.top() + 3.0 + i as f32 * 4.0;
                painter.rect_filled(
                    egui::Rect::from_min_size(egui::pos2(rect.left() + 2.0, y), egui::vec2(3.0, 2.5)),
                    Rounding::ZERO,
                    Color32::from_rgb(220, 220, 220),
                );
                painter.rect_filled(
                    egui::Rect::from_min_size(egui::pos2(rect.right() - 5.0, y), egui::vec2(3.0, 2.5)),
                    Rounding::ZERO,
                    Color32::from_rgb(220, 220, 220),
                );
            }
        }
        CatIcon::Checked => {
            painter.circle_filled(rect.center(), 7.0, Color32::from_rgb(0, 150, 0));
            painter.line_segment([rect.center() + egui::vec2(-3.0, 0.0), rect.center() + egui::vec2(-1.0, 2.5)], Stroke::new(1.6_f32, Color32::WHITE));
            painter.line_segment([rect.center() + egui::vec2(-1.0, 2.5), rect.center() + egui::vec2(3.0, -3.0)], Stroke::new(1.6_f32, Color32::WHITE));
        }
    }
}

fn draw_file_icon(painter: &egui::Painter, rect: egui::Rect, name: &str) {
    let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    let color = match ext.as_str() {
        "zip" | "7z" | "rar" | "gz" | "tar" => Color32::from_rgb(230, 190, 60),
        "exe" | "msi" => Color32::from_rgb(120, 140, 190),
        "pdf" => Color32::from_rgb(200, 40, 40),
        "mp3" | "m4a" | "flac" | "wav" => Color32::from_rgb(80, 120, 220),
        "mp4" | "mkv" | "avi" | "webm" => Color32::from_rgb(90, 160, 90),
        "doc" | "docx" | "txt" => Color32::from_rgb(60, 90, 180),
        "torrent" => Color32::from_rgb(60, 160, 160),
        _ => Color32::from_rgb(150, 160, 175),
    };
    let tinted = Color32::from_rgb(
        (color.r() as f32 * 0.45 + 255.0 * 0.55) as u8,
        (color.g() as f32 * 0.45 + 255.0 * 0.55) as u8,
        (color.b() as f32 * 0.45 + 255.0 * 0.55) as u8,
    );
    painter.rect_filled(rect, Rounding::same(1.0_f32), tinted);
    painter.rect_stroke(rect, Rounding::same(1.0_f32), Stroke::new(1.0_f32, color));
    // folded corner
    let fold = 4.0;
    painter.add(egui::Shape::convex_polygon(
        vec![
            egui::pos2(rect.right() - fold, rect.top()),
            egui::pos2(rect.right(), rect.top() + fold),
            egui::pos2(rect.right() - fold, rect.top() + fold),
        ],
        Color32::WHITE,
        Stroke::new(0.8_f32, color),
    ));
    painter.line_segment(
        [egui::pos2(rect.left() + 3.0, rect.bottom() - 5.0), egui::pos2(rect.right() - 3.0, rect.bottom() - 5.0)],
        Stroke::new(0.8_f32, color),
    );
    painter.line_segment(
        [egui::pos2(rect.left() + 3.0, rect.bottom() - 3.0), egui::pos2(rect.right() - 5.0, rect.bottom() - 3.0)],
        Stroke::new(0.8_f32, color),
    );
}

fn draw_status_icon(painter: &egui::Painter, rect: egui::Rect, status: Status, running: bool) {
    let c = rect.center();
    match status {
        Status::Completed => {
            painter.circle_filled(c, 6.5, Color32::from_rgb(0, 150, 0));
            painter.line_segment([c + egui::vec2(-3.0, 0.0), c + egui::vec2(-1.0, 2.5)], Stroke::new(1.6_f32, Color32::WHITE));
            painter.line_segment([c + egui::vec2(-1.0, 2.5), c + egui::vec2(3.0, -3.0)], Stroke::new(1.6_f32, Color32::WHITE));
        }
        Status::Error => {
            painter.circle_filled(c, 6.5, Color32::from_rgb(200, 30, 30));
            painter.line_segment([c + egui::vec2(-2.5, -2.5), c + egui::vec2(2.5, 2.5)], Stroke::new(1.6_f32, Color32::WHITE));
            painter.line_segment([c + egui::vec2(2.5, -2.5), c + egui::vec2(-2.5, 2.5)], Stroke::new(1.6_f32, Color32::WHITE));
        }
        Status::Paused => {
            painter.rect_filled(egui::Rect::from_center_size(c + egui::vec2(-2.0, 0.0), egui::vec2(2.5, 8.0)), Rounding::ZERO, Color32::from_rgb(190, 110, 0));
            painter.rect_filled(egui::Rect::from_center_size(c + egui::vec2(2.0, 0.0), egui::vec2(2.5, 8.0)), Rounding::ZERO, Color32::from_rgb(190, 110, 0));
        }
        Status::Queued => {
            painter.circle_filled(c, 6.0, Color32::from_rgb(140, 150, 165));
            painter.line_segment([c, c + egui::vec2(0.0, -3.5)], Stroke::new(1.2_f32, Color32::WHITE));
            painter.line_segment([c, c + egui::vec2(2.5, 1.0)], Stroke::new(1.2_f32, Color32::WHITE));
        }
        Status::Connecting => {
            painter.circle_stroke(c, 5.0, Stroke::new(1.8_f32, Color32::from_rgb(0, 110, 200)));
        }
        Status::Downloading => {
            painter.circle_filled(c, 6.5, Color32::from_rgb(0, 110, 200));
            painter.line_segment([c + egui::vec2(0.0, -3.5), c + egui::vec2(0.0, 3.0)], Stroke::new(1.6_f32, Color32::WHITE));
            painter.line_segment([c + egui::vec2(-2.0, 1.0), c + egui::vec2(0.0, 3.0)], Stroke::new(1.6_f32, Color32::WHITE));
            painter.line_segment([c + egui::vec2(2.0, 1.0), c + egui::vec2(0.0, 3.0)], Stroke::new(1.6_f32, Color32::WHITE));
        }
        Status::Canceled => {
            painter.circle_filled(c, 6.0, Color32::from_rgb(150, 150, 150));
        }
    }
    if running && matches!(status, Status::Paused | Status::Queued) {
        painter.circle_filled(c, 6.5, Color32::from_rgb(0, 110, 200));
    }
}

fn draw_connection_row(ui: &mut egui::Ui, index: usize, segment: &crate::model::Segment) {
    let width = ui.available_width().max(320.0);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, 27.0), egui::Sense::hover());
    let painter = ui.painter();
    let background = if index % 2 == 0 {
        Color32::WHITE
    } else {
        Color32::from_rgb(247, 249, 252)
    };
    painter.rect_filled(rect, Rounding::ZERO, background);
    painter.rect_filled(
        egui::Rect::from_min_max(
            egui::pos2(rect.left(), rect.bottom() - 1.0),
            rect.right_bottom(),
        ),
        Rounding::ZERO,
        Color32::from_rgb(235, 239, 245),
    );

    let badge = egui::Rect::from_center_size(
        egui::pos2(rect.left() + 23.0, rect.center().y),
        egui::vec2(30.0, 16.0),
    );
    painter.rect_filled(badge, Rounding::same(3.0), Color32::from_rgb(224, 234, 247));
    painter.text(
        badge.center(),
        Align2::CENTER_CENTER,
        format!("#{}", index + 1),
        FontId::proportional(9.5),
        BLUE,
    );

    let bar_left = rect.left() + 46.0;
    let bar_right = (rect.right() - 146.0).max(bar_left + 30.0);
    let bar = egui::Rect::from_min_max(
        egui::pos2(bar_left, rect.center().y - 7.0),
        egui::pos2(bar_right, rect.center().y + 7.0),
    );
    painter.rect_filled(bar, Rounding::same(3.0), Color32::from_rgb(231, 235, 242));

    let total = segment.len();
    let done = segment.done.min(total);
    let fraction = if total > 0 {
        done as f32 / total as f32
    } else {
        0.0
    };
    if fraction > 0.0 {
        let filled = egui::Rect::from_min_size(
            bar.min,
            egui::vec2(bar.width() * fraction, bar.height()),
        );
        let clipped = painter.with_clip_rect(filled);
        let slices = 80;
        for i in 0..slices {
            let t0 = i as f32 / slices as f32;
            let t1 = (i + 1) as f32 / slices as f32;
            let x0 = bar.left() + bar.width() * t0;
            let x1 = (bar.left() + bar.width() * t1 + 0.5).min(bar.right());
            let color = Color32::from_rgb(
                (47.0 + (56.0 - 47.0) * t0) as u8,
                (123.0 + (183.0 - 123.0) * t0) as u8,
                (214.0 + (196.0 - 214.0) * t0) as u8,
            );
            clipped.rect_filled(
                egui::Rect::from_min_max(
                    egui::pos2(x0, bar.top()),
                    egui::pos2(x1, bar.bottom()),
                ),
                Rounding::ZERO,
                color,
            );
        }
        let gloss = egui::Rect::from_min_max(
            bar.min,
            egui::pos2(bar.right(), bar.top() + bar.height() * 0.42),
        )
        .intersect(filled);
        painter.rect_filled(gloss, Rounding::ZERO, Color32::from_white_alpha(28));
    }

    painter.text(
        bar.center(),
        Align2::CENTER_CENTER,
        format!("{:.0}%", fraction * 100.0),
        FontId::proportional(9.0),
        if fraction > 0.55 {
            Color32::WHITE
        } else {
            Color32::from_rgb(45, 55, 68)
        },
    );
    painter.text(
        egui::pos2(rect.right() - 7.0, rect.center().y),
        Align2::RIGHT_CENTER,
        format!("{} – {}", segment.start, segment.end.saturating_sub(1)),
        FontId::proportional(9.0),
        DIM,
    );
}

/// Refined rainbow progress bar: muted, smoothly blended spectrum, a
/// progress-reactive "drag" tail, and no motion once the transfer is frozen or
/// complete.
fn draw_rainbow_bar(
    ui: &mut egui::Ui,
    frac: f32,
    indeterminate: bool,
    animate: bool,
    anim_phase: f32,
    label: &str,
) {
    let width = ui.available_width().max(240.0);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, 30.0), egui::Sense::hover());
    let painter = ui.painter();
    let outer = rect.shrink(1.0);
    painter.rect_filled(outer, Rounding::same(6.0), Color32::from_rgb(240, 243, 247));
    painter.rect_stroke(
        outer,
        Rounding::same(6.0),
        Stroke::new(1.0, Color32::from_rgb(176, 182, 192)),
    );

    let inner = outer.shrink(2.0);
    let fill = if indeterminate {
        0.36
    } else {
        frac.clamp(0.0, 1.0)
    };
    let filled = egui::Rect::from_min_size(
        inner.min,
        egui::vec2(inner.width() * fill, inner.height()),
    );

    // very soft tinted track: same spectrum, heavily blended toward white so
    // the empty part never looks striped or high-contrast
    const SLICES: usize = 280;
    for i in 0..SLICES {
        let t0 = i as f32 / SLICES as f32;
        let t1 = (i + 1) as f32 / SLICES as f32;
        let x0 = inner.left() + inner.width() * t0;
        let x1 = (inner.left() + inner.width() * t1 + 0.6).min(inner.right());
        let seg = egui::Rect::from_min_max(
            egui::pos2(x0, inner.top()),
            egui::pos2(x1, inner.bottom()),
        );
        painter.rect_filled(seg, Rounding::ZERO, lighten(rainbow_color(t0), 0.72));
    }

    if fill > 0.0 {
        let clipped = painter.with_clip_rect(filled);
        // full muted spectrum compressed into the filled width; 240 overlapping
        // slices avoid visible separation between colours
        let steps = 240;
        for i in 0..steps {
            let t = i as f32 / (steps - 1) as f32;
            let x0 = filled.left() + filled.width() * (i as f32 / steps as f32);
            let x1 = (filled.left()
                + filled.width() * ((i + 1) as f32 / steps as f32)
                + 0.6)
                .min(filled.right());
            let seg = egui::Rect::from_min_max(
                egui::pos2(x0, inner.top()),
                egui::pos2(x1, inner.bottom()),
            );
            clipped.rect_filled(seg, Rounding::ZERO, rainbow_color(t));
        }

        // glassy top highlight and a quiet bottom shade
        let gloss = egui::Rect::from_min_max(
            inner.min,
            egui::pos2(inner.right(), inner.top() + inner.height() * 0.42),
        )
        .intersect(filled);
        painter.rect_filled(gloss, Rounding::ZERO, Color32::from_white_alpha(26));
        let shade = egui::Rect::from_min_max(
            egui::pos2(inner.left(), inner.bottom() - inner.height() * 0.18),
            egui::pos2(inner.right(), inner.bottom()),
        )
        .intersect(filled);
        painter.rect_filled(shade, Rounding::ZERO, Color32::from_black_alpha(10));

        if animate && !indeterminate {
            let p = fill.max(0.01);
            let pulse =
                ((anim_phase * std::f32::consts::TAU).sin() * 0.5 + 0.5).clamp(0.0, 1.0);

            for k in 0..2 {
                // integer frequency: wraps seamlessly when phase reaches 1
                let at = (anim_phase * 2.0 + k as f32 * 0.5).rem_euclid(1.0);
                let x = filled.left() + filled.width() * at;
                let band = egui::Rect::from_min_max(
                    egui::pos2(x - 22.0, inner.top()),
                    egui::pos2(x + 22.0, inner.bottom()),
                )
                .intersect(filled);
                clipped.rect_filled(
                    band,
                    Rounding::ZERO,
                    Color32::from_rgba_unmultiplied(200, 240, 255, 16),
                );
            }

            // the leading edge drags a soft tail behind it; the tail lengthens
            // with progress so a full bar feels fast and fluid
            let tail_len = (16.0 + 54.0 * p).min(filled.width());
            let tail = egui::Rect::from_min_max(
                egui::pos2(filled.right() - tail_len, inner.top()),
                egui::pos2(filled.right(), inner.bottom()),
            );
            let tail_steps = 28;
            for i in 0..tail_steps {
                let t = i as f32 / (tail_steps - 1) as f32;
                let x0 = tail.left() + tail.width() * (i as f32 / tail_steps as f32);
                let x1 = (tail.left()
                    + tail.width() * ((i + 1) as f32 / tail_steps as f32)
                    + 0.5)
                    .min(tail.right());
                let seg = egui::Rect::from_min_max(
                    egui::pos2(x0, inner.top()),
                    egui::pos2(x1, inner.bottom()),
                );
                clipped.rect_filled(
                    seg,
                    Rounding::ZERO,
                    Color32::from_rgba_unmultiplied(
                        190,
                        235,
                        255,
                        (3.0 + 24.0 * t * t) as u8,
                    ),
                );
            }
            // glowing turbo tip: pulsing halo, arrowhead and speed streaks that
            // lunge forward as if forcing the download ahead
            let lurch = (1.5 + 7.0 * p) * pulse;
            let tip_x = (filled.right() + lurch).min(inner.right() - 2.0);
            let tip = egui::pos2(tip_x, inner.center().y);
            let tip_clip = painter.with_clip_rect(outer.shrink(1.0));

            for k in 0..3 {
                let dy = (k as f32 - 1.0) * 6.0;
                let len = 14.0 + 12.0 * pulse + 10.0 * p + k as f32 * 5.0;
                let x0 = (tip_x - len).max(inner.left());
                let x1 = (tip_x - 3.0).max(x0);
                tip_clip.line_segment(
                    [egui::pos2(x0, tip.y + dy), egui::pos2(x1, tip.y + dy)],
                    Stroke::new(
                        1.4,
                        Color32::from_rgba_unmultiplied(
                            180,
                            232,
                            255,
                            (22.0 + 22.0 * pulse) as u8,
                        ),
                    ),
                );
            }

            // glow bleeding into the empty part ahead of the tip: this is what
            // makes it look like the download is forcing the bar forward
            let ahead = 12.0 + 8.0 * pulse;
            let ahead_steps = 12;
            for i in 0..ahead_steps {
                let t = i as f32 / ahead_steps as f32;
                let x0 = filled.right() + ahead * t;
                let x1 = filled.right() + ahead * ((i + 1) as f32 / ahead_steps as f32);
                if x0 >= inner.right() {
                    break;
                }
                let alpha = ((1.0 - t) * (1.0 - t) * 58.0) as u8;
                let seg = egui::Rect::from_min_max(
                    egui::pos2(x0, inner.top()),
                    egui::pos2(x1.min(inner.right()), inner.bottom()),
                );
                tip_clip.rect_filled(
                    seg,
                    Rounding::ZERO,
                    Color32::from_rgba_unmultiplied(80, 205, 255, alpha),
                );
            }

            // two cyan chevrons run out ahead of the tip and fade, as if the
            // download is pushing the bar forward
            for k in 0..2 {
                let cp = (anim_phase * 2.0 + k as f32 * 0.5).rem_euclid(1.0);
                let cx = tip_x + 8.0 + cp * 20.0;
                if cx + 6.0 < inner.right() {
                    let alpha = ((1.0 - cp) * (50.0 + 35.0 * pulse)) as u8;
                    let color = Color32::from_rgba_unmultiplied(120, 225, 255, alpha);
                    tip_clip.line_segment(
                        [egui::pos2(cx, tip.y - 6.0), egui::pos2(cx + 5.0, tip.y)],
                        Stroke::new(2.0, color),
                    );
                    tip_clip.line_segment(
                        [egui::pos2(cx + 5.0, tip.y), egui::pos2(cx, tip.y + 6.0)],
                        Stroke::new(2.0, color),
                    );
                }
            }

            let glow_r = 18.0 + 9.0 * pulse + 8.0 * p;
            for (r, a) in [(glow_r, 22u8), (glow_r * 0.62, 45), (glow_r * 0.36, 90)] {
                tip_clip.circle_filled(
                    tip,
                    r,
                    Color32::from_rgba_unmultiplied(80, 205, 255, a),
                );
            }
            // only a tiny white pin-prick; the tip stays cyan instead of being
            // covered by a large white cap
            tip_clip.circle_filled(
                tip,
                1.7 + 0.5 * pulse,
                Color32::from_rgba_unmultiplied(245, 252, 255, 220),
            );

            let s = 0.72 + 0.24 * pulse;
            let arrow = vec![
                egui::pos2(tip.x + 3.8 * s, tip.y),
                egui::pos2(tip.x - 2.8 * s, tip.y - 4.6 * s),
                egui::pos2(tip.x - 2.8 * s, tip.y + 4.6 * s),
            ];
            tip_clip.add(egui::Shape::convex_polygon(
                arrow,
                Color32::from_rgba_unmultiplied(60, 190, 240, 235),
                Stroke::new(
                    0.8,
                    Color32::from_rgba_unmultiplied(220, 250, 255, 120),
                ),
            ));
        }
    }

    if animate && indeterminate {
        let x = inner.left() + inner.width() * anim_phase;
        let stripe = egui::Rect::from_min_max(
            egui::pos2(x - 26.0, inner.top()),
            egui::pos2(x + 26.0, inner.bottom()),
        )
        .intersect(inner);
        painter.rect_filled(
            stripe,
            Rounding::ZERO,
            Color32::from_rgba_unmultiplied(200, 240, 255, 20),
        );
    }

    // no label background: contrast-aware text with a soft shadow instead of a
    // white box sitting on top of the bar
    let galley = painter.layout_no_wrap(
        label.to_string(),
        FontId::proportional(11.0),
        Color32::BLACK,
    );
    let text_pos = inner.center() - galley.size() / 2.0;
    let (fg, shadow) = if fill > 0.55 {
        (Color32::WHITE, Color32::from_black_alpha(110))
    } else {
        (
            Color32::from_rgb(38, 42, 48),
            Color32::from_white_alpha(160),
        )
    };
    painter.galley(text_pos + egui::vec2(0.0, 1.0), galley.clone(), shadow);
    painter.galley(text_pos, galley, fg);
}

fn rainbow_color(t: f32) -> Color32 {
    // muted, consistent-lightness spectrum (no primary red/green/blue jumps)
    const STOPS: [(u8, u8, u8); 9] = [
        (226, 111, 97),
        (232, 154, 89),
        (222, 190, 111),
        (161, 194, 133),
        (108, 184, 162),
        (92, 170, 190),
        (101, 142, 199),
        (126, 121, 187),
        (170, 113, 168),
    ];
    let t = t.clamp(0.0, 1.0);
    let scaled = t * (STOPS.len() - 1) as f32;
    let i = (scaled.floor() as usize).min(STOPS.len() - 2);
    let mut f = scaled - i as f32;
    f = f * f * (3.0 - 2.0 * f); // smoothstep removes the visible stop seams
    let a = STOPS[i];
    let b = STOPS[i + 1];
    Color32::from_rgb(
        (a.0 as f32 + (b.0 as f32 - a.0 as f32) * f) as u8,
        (a.1 as f32 + (b.1 as f32 - a.1 as f32) * f) as u8,
        (a.2 as f32 + (b.2 as f32 - a.2 as f32) * f) as u8,
    )
}


fn lighten(color: Color32, amount: f32) -> Color32 {
    let amount = amount.clamp(0.0, 1.0);
    let lift = |v: u8| (v as f32 + (255.0 - v as f32) * amount) as u8;
    Color32::from_rgb(lift(color.r()), lift(color.g()), lift(color.b()))
}

fn row_status(status: Status, running: bool, pct: f64) -> (String, Color32) {
    if running {
        if status == Status::Completed {
            return ("Complete".into(), GREEN);
        }
        return (format!("{:.2}%", pct), BLUE);
    }
    match status {
        Status::Completed => ("Complete".into(), GREEN),
        Status::Error => ("Error".into(), RED),
        Status::Canceled => ("Canceled".into(), DIM),
        Status::Paused => ("Paused".into(), ORANGE),
        Status::Queued => ("Queued".into(), DIM),
        Status::Connecting => ("Connecting...".into(), BLUE),
        Status::Downloading => (format!("{:.2}%", pct), BLUE),
    }
}

fn open_file(path: &std::path::Path) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let _ = std::process::Command::new("cmd")
            .args(["/C", "start", ""])
            .arg(path)
            .creation_flags(0x0800_0000)
            .spawn();
    }
    #[cfg(not(windows))]
    {
        let _ = std::process::Command::new("xdg-open").arg(path).spawn();
    }
}

fn open_folder(path: &std::path::Path) {
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("explorer").arg(path).spawn();
    }
    #[cfg(not(windows))]
    {
        let _ = std::process::Command::new("xdg-open").arg(path).spawn();
    }
}

fn col_widths(total: f32) -> [f32; 8] {
    let q = 22.0;
    let size = 92.0;
    let status = 96.0;
    let time = 80.0;
    let rate = 100.0;
    let last = 120.0;
    let name = 172.0;
    let desc = (total - (q + size + status + time + rate + last) - name).max(150.0);
    [name, q, size, status, time, rate, last, desc]
}

/// "Oct 20 '26" from a unix timestamp (UTC), used in the Last Try column.
fn format_date(ts: u64) -> String {
    if ts == 0 {
        return String::new();
    }
    let days = (ts / 86_400) as i64;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = mp + if mp < 10 { 3 } else { -9 };
    let year = if m <= 2 { y + 1 } else { y };
    let months = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    format!("{} {} '{}", months[(m - 1) as usize], d, year % 100)
}

pub fn run() -> Result<()> {
    let start_in_tray = std::env::args().any(|arg| arg == "--tray" || arg == "--startup");
    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([1125.0, 680.0])
        .with_min_inner_size([900.0, 500.0])
        .with_visible(!start_in_tray)
        .with_title("TDM - The Download Manager");
    if let Some(icon) = app_icon_data() {
        viewport = viewport.with_icon(std::sync::Arc::new(icon));
    }
    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };
    eframe::run_native(
        "TDM",
        options,
        Box::new(|cc| Ok(Box::new(TdmApp::new(cc)))),
    )
    .map_err(|e| anyhow::anyhow!("GUI failed: {e}"))?;
    Ok(())
}
