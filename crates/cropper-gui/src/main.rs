//! Cropper — 스캔 PDF 여백 자르기 (egui + eframe).

use eframe::egui;
use egui::{Color32, CornerRadius, Sense, Stroke, StrokeKind, Vec2};
use cropper_core::plan::{plan_crops, CropMode, PlanOptions, TargetSize};
use cropper_core::session::Session;
use cropper_core::types::{CropRect, DetectOptions, PageAnalysis, PaperSize};

const MAX_PX: u32 = 1000;
const CELL_WIDTH: f32 = 172.0;
/// Crop handle square size and the padding kept around the page so edge
/// handles stay fully visible and grabbable.
/// Long-side pixel sizes of the sharp preview render (smallest one that covers
/// the on-screen size is used) and how long the view must rest before rendering.
const DETAIL_STEPS: [u32; 4] = [1600, 2400, 3200, 4000];
const DETAIL_DEBOUNCE: f64 = 0.25;
const HANDLE_SIZE: f32 = 12.0;
const HANDLE_PAD: f32 = 16.0;
const MM_PER_PT: f32 = 25.4 / 72.0;
/// Quiet time after the last detection-setting change before re-detecting.
const REDETECT_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(150);

/// One page's data + UI state.
struct PageData {
    analysis: Option<PageAnalysis>,
    /// Manual crop override set by the user; wins over the detector.
    manual: Option<CropRect>,
    enabled: bool,
    texture: Option<egui::TextureHandle>,
}

/// Which part of the crop box is being grabbed in the preview.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Handle {
    Inside,
    Left,
    Right,
    Top,
    Bottom,
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

/// Active manual crop drag in the preview.
#[derive(Debug, Clone, Copy)]
struct CropDrag {
    handle: Handle,
    index: usize,
    /// Pointer position at drag start, image-local px.
    start_px: egui::Vec2,
    start_crop: CropRect,
}

/// Slider/radio settings.
struct Settings {
    threshold: u8,
    pad_mm: f32,
    min_ink: u32,
    edge_mm: f32,
    uniform: bool,
    /// Content anchor inside the paper margins: (h, v), each 0 = start, 1 = center, 2 = end.
    align: (u8, u8),
    /// Fixed output paper; `None` = free page size per crop.
    paper: Option<PaperSize>,
}

#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
/// Run `job` on a background thread: a std thread natively, a pooled web
/// worker (tokio_with_wasm, needs the atomics build) on the web.
fn spawn_job(job: impl FnOnce() + Send + 'static) {
    #[cfg(not(target_arch = "wasm32"))]
    {
        std::thread::spawn(job);
    }
    #[cfg(target_arch = "wasm32")]
    {
        drop(tokio_with_wasm::task::spawn_blocking(job));
    }
}

/// How many pages render concurrently.
fn scan_workers() -> usize {
    // Scans with huge image masks peak at 0.5-0.7 GiB per page render, and
    // wasm memory never shrinks, so the web build stays at two workers (the
    // heap high-water mark scales with concurrency).
    #[cfg(target_arch = "wasm32")]
    return 2;
    // Native: leave one core for the UI.
    #[cfg(not(target_arch = "wasm32"))]
    std::thread::available_parallelism().map_or(2, |n| n.get()).saturating_sub(1).clamp(1, 8)
}

/// A page rendered by a worker, tagged with the load it belongs to.
struct Scanned {
    generation: u64,
    index: usize,
    raster: Result<cropper_core::render::PageRaster, String>,
}

#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
enum Inbox {
    Opened(String, Vec<u8>),
}

struct DetailTex {
    index: usize,
    px: u32,
    texture: egui::TextureHandle,
}

struct App {
    session: Option<Session>,
    file_name: String,
    pages: Vec<PageData>,
    settings: Settings,
    /// Settings the current detections were made with; `None` until the
    /// first full scan completed.
    applied: Option<Settings>,
    /// Number of pages already rendered + analyzed (scan is sequential).
    scanned: usize,
    /// Page shown in the center editor.
    selected: usize,
    /// Center shows the all-pages overview grid instead of the editor.
    overview: bool,
    /// Scroll the left sidebar to the selected page on the next frame.
    scroll_to_selected: bool,
    /// Preview zoom factor (pinch/buttons), 1.0 = fit.
    preview_zoom: f32,
    /// Thumbnail scale in the overview grid (pinch / slider).
    overview_zoom: f32,
    /// High-resolution render of one page for the zoomed preview.
    detail: Option<DetailTex>,
    /// (page, px, since) wanted detail render waiting out the debounce.
    detail_wait: Option<(usize, u32, f64)>,
    /// Current / requested scroll offset of the editor (for cursor-anchored zoom).
    preview_offset: Vec2,
    pending_offset: Option<Vec2>,
    /// When detection settings last diverged from `applied` (debounce).
    settings_dirty_since: Option<f64>,
    /// In-progress manual crop edit in the preview.
    crop_drag: Option<CropDrag>,
    error: Option<String>,
    /// File picked/dropped asynchronously (web), delivered next frame.
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    inbox_tx: std::sync::mpsc::Sender<Inbox>,
    inbox_rx: std::sync::mpsc::Receiver<Inbox>,
    /// Parallel page rendering: workers send finished rasters here.
    scan_tx: std::sync::mpsc::Sender<Scanned>,
    scan_rx: std::sync::mpsc::Receiver<Scanned>,
    /// Bumped on every load so late results of a previous file are dropped.
    scan_generation: u64,
    /// Next page index to hand to a worker.
    scan_dispatched: usize,
    /// Rasters that finished ahead of `scanned` (completion is out of order).
    scan_ready: std::collections::HashMap<usize, cropper_core::render::PageRaster>,
}

impl App {
    fn new() -> Self {
        let (inbox_tx, inbox_rx) = std::sync::mpsc::channel();
        let (scan_tx, scan_rx) = std::sync::mpsc::channel();
        Self {
            scan_tx,
            scan_rx,
            scan_generation: 0,
            scan_dispatched: 0,
            scan_ready: std::collections::HashMap::new(),
            session: None,
            file_name: String::new(),
            pages: Vec::new(),
            settings: Settings {
                threshold: 200,
                pad_mm: 10.0,
                min_ink: 3,
                edge_mm: 3.0,
                uniform: false,
                align: (1, 1),
                paper: None,
            },
            applied: None,
            scanned: 0,
            selected: 0,
            overview: false,
            scroll_to_selected: false,
            preview_zoom: 1.0,
            overview_zoom: 1.0,
            detail: None,
            detail_wait: None,
            preview_offset: Vec2::ZERO,
            pending_offset: None,
            settings_dirty_since: None,
            crop_drag: None,
            error: None,
            inbox_tx,
            inbox_rx,
        }
    }

    /// Default options from a URL query string (`?paper=b4&pad=10&align=rt`).
    /// Keys: `threshold`, `pad` (mm), `noise`, `edge` (mm), `uniform` (1/0),
    /// `paper` (auto|a3|a4|a5|b4|b5|letter|legal), `align` (`[l|c|r][t|c|b]`),
    /// `view` (`page`|`overview`). Unknown keys and bad values are ignored.
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    fn apply_query(&mut self, query: &str) {
        let st = &mut self.settings;
        for pair in query.trim_start_matches('?').split('&') {
            let Some((key, value)) = pair.split_once('=') else { continue };
            let value = value.trim();
            match key {
                "threshold" => {
                    if let Ok(v) = value.parse::<u8>() {
                        st.threshold = v.clamp(120, 250);
                    }
                }
                "pad" => {
                    if let Ok(v) = value.parse::<f32>() {
                        st.pad_mm = v.max(0.0);
                    }
                }
                "noise" => {
                    if let Ok(v) = value.parse::<u32>() {
                        st.min_ink = v.clamp(1, 25);
                    }
                }
                "edge" => {
                    if let Ok(v) = value.parse::<f32>() {
                        st.edge_mm = v.max(0.0);
                    }
                }
                "uniform" => st.uniform = matches!(value, "1" | "true" | "on" | "yes"),
                "paper" => {
                    if value.eq_ignore_ascii_case("auto") {
                        st.paper = None;
                    } else if let Some(p) = PaperSize::parse(value) {
                        st.paper = Some(p);
                    }
                }
                "align" => {
                    let b: Vec<char> = value.to_lowercase().chars().collect();
                    let h = match b.first() {
                        Some('l') => Some(0),
                        Some('c') => Some(1),
                        Some('r') => Some(2),
                        _ => None,
                    };
                    let v = match b.get(1) {
                        Some('t') => Some(0),
                        Some('c') => Some(1),
                        Some('b') => Some(2),
                        _ => None,
                    };
                    if let (Some(h), Some(v)) = (h, v) {
                        st.align = (h, v);
                    }
                }
                "view" => self.overview = value == "overview",
                _ => {}
            }
        }
    }

    fn load(&mut self, bytes: Vec<u8>, name: String) {
        // macOS hands out decomposed (NFD) Hangul file names; the font only has
        // precomposed syllables, so normalize or they render as tofu.
        let name: String = unicode_normalization::UnicodeNormalization::nfc(name.as_str()).collect();
        match Session::new(bytes) {
            Ok(session) => {
                self.pages = (0..session.page_count())
                    .map(|_| PageData {
                        analysis: None,
                        manual: None,
                        enabled: true,
                        texture: None,
                    })
                    .collect();
                self.session = Some(session);
                self.file_name = name;
                self.scanned = 0;
                self.scan_generation += 1;
                self.scan_dispatched = 0;
                self.scan_ready.clear();
                self.applied = None;
                self.selected = 0;
                self.preview_zoom = 1.0;
                self.detail = None;
                self.detail_wait = None;
                self.crop_drag = None;
                self.error = None;
            }
            Err(err) => self.error = Some(err),
        }
    }

    /// Re-detect every already-scanned page with the current settings.
    /// Rasters stay cached in the session, so this skips re-rendering.
    fn reapply(&mut self) {
        if self.scanned == 0 {
            return;
        }
        let options = DetectOptions {
            threshold: self.settings.threshold,
            min_ink: self.settings.min_ink,
            edge_mm: self.settings.edge_mm,
        };
        let session = self.session.as_mut().unwrap();
        for index in 0..self.scanned {
            let (crop, ratio, blank, _) = match session.analyze(index, MAX_PX, &options) {
                Ok(v) => v,
                Err(e) => {
                    self.error = Some(e);
                    return;
                }
            };
            let (page_width, page_height) = session.page_size(index).unwrap_or_default();
            self.pages[index].analysis = Some(PageAnalysis {
                page_width,
                page_height,
                crop,
                ink_ratio: ratio,
                blank,
            });
        }
        self.applied = Some(Settings {
            threshold: self.settings.threshold,
            pad_mm: self.settings.pad_mm,
            min_ink: self.settings.min_ink,
            edge_mm: self.settings.edge_mm,
            uniform: self.settings.uniform,
            align: self.settings.align,
            paper: self.settings.paper,
        });
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // Trackpad pinch is handled inside edit_crop_interact, anchored to
        // the preview image, so palm contact elsewhere does nothing.

        while let Ok(msg) = self.inbox_rx.try_recv() {
            match msg {
                Inbox::Opened(name, bytes) => self.load(bytes, name),
            }
        }
        self.handle_dropped_files(ui);
        self.scan_next(ui);
        self.top_bar(ui);
        self.bottom_bar(ui);
        self.sidebars(ui);
        self.center(ui);
        self.modal_windows(ui);

        if self.scanned < self.pages.len() {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(30));
        }
    }
}

impl App {
    fn handle_dropped_files(&mut self, ui: &mut egui::Ui) {
        let dropped = ui.ctx().input(|i| i.raw.dropped_files.clone());
        let Some(file) = dropped.first() else { return };
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(path) = &file.path {
            if let Ok(bytes) = std::fs::read(path) {
                let name = path
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "문서.pdf".into());
                self.load(bytes, name);
            }
            return;
        }
        if let Some(bytes) = &file.bytes {
            let name = if file.name.is_empty() { "끌어다 놓은 파일.pdf".into() } else { file.name.clone() };
            self.load(bytes.to_vec(), name);
        }
    }

    /// Keep `scan_workers()` page renders in flight on background threads,
    /// then consume finished pages in order (detection is cheap; rendering is
    /// the cost). 70-page scans render in parallel instead of one per frame.
    fn scan_next(&mut self, ui: &mut egui::Ui) {
        let Some(session) = &mut self.session else { return };
        let total = session.page_count();
        if self.scanned >= total {
            return;
        }

        // Collect finished renders.
        while let Ok(done) = self.scan_rx.try_recv() {
            if done.generation != self.scan_generation {
                continue;
            }
            match done.raster {
                Ok(raster) => {
                    self.scan_ready.insert(done.index, raster);
                }
                Err(e) => {
                    self.error = Some(e);
                    self.scanned = total;
                    return;
                }
            }
        }

        // Top up the workers, bounding how far dispatch runs ahead of the
        // consumed prefix so buffered rasters stay few.
        let workers = scan_workers();
        let window = self.scanned + workers * 2;
        let in_flight = self.scan_dispatched - self.scanned - self.scan_ready.len();
        let mut free = workers.saturating_sub(in_flight);
        while free > 0 && self.scan_dispatched < total.min(window) {
            let (pdf, index, generation) = (session.pdf(), self.scan_dispatched, self.scan_generation);
            let tx = self.scan_tx.clone();
            // Workers must not touch egui on the web: eframe looks up
            // `window()` (absent in a worker) and parking_lot cannot park.
            // The 30 ms repaint poll while scanning picks results up instead.
            #[cfg(not(target_arch = "wasm32"))]
            let ctx = ui.ctx().clone();
            spawn_job(move || {
                let raster = cropper_core::session::render_page(&pdf, index, MAX_PX)
                    .ok_or_else(|| format!("페이지 번호가 범위를 벗어났습니다: {index}"));
                let _ = tx.send(Scanned { generation, index, raster });
                #[cfg(not(target_arch = "wasm32"))]
                ctx.request_repaint();
            });
            self.scan_dispatched += 1;
            free -= 1;
        }

        // Consume the contiguous finished prefix.
        let options =
            DetectOptions { threshold: self.settings.threshold, min_ink: self.settings.min_ink, edge_mm: self.settings.edge_mm };
        let mut consumed = 0;
        while consumed < 4 {
            let index = self.scanned;
            let Some(raster) = self.scan_ready.remove(&index) else { break };
            session.insert_raster(index, MAX_PX, raster);
            let texture = make_texture(ui.ctx(), index, session.raster(index).unwrap());
            let fallback = CropRect::new(0.0, 0.0, 1.0, 1.0);
            let (crop, ratio, blank, _) =
                session.analyze(index, MAX_PX, &options).unwrap_or((fallback, 0.0, true, 1));
            let (page_width, page_height) = session.page_size(index).unwrap_or_default();
            let page = &mut self.pages[index];
            page.analysis = Some(PageAnalysis { page_width, page_height, crop, ink_ratio: ratio, blank });
            page.texture = Some(texture);
            self.scanned += 1;
            consumed += 1;
            if self.scanned == total {
                break;
            }
        }
        if consumed > 0 && self.scanned == total {
            self.finish_scan(total);
        }
    }

    fn finish_scan(&mut self, total: usize) {
        {
            self.applied = Some(Settings {
                threshold: self.settings.threshold,
                pad_mm: self.settings.pad_mm,
                min_ink: self.settings.min_ink,
                edge_mm: self.settings.edge_mm,
                uniform: self.settings.uniform,
                align: self.settings.align,
                paper: self.settings.paper,
            });
            // Demo helper: CROPPER_PREVIEW=n opens the big preview on page n.
            if std::env::var("CROPPER_OVERVIEW").is_ok() {
                self.overview = true;
            }
            if let Ok(n) = std::env::var("CROPPER_PREVIEW") {
                if let Ok(n) = n.parse::<usize>() {
                    if n >= 1 && n <= total {
                        self.selected = n - 1;
                    }
                }
            }
        }
    }

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::top("top_bar").show_inside(ui, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                if ui.button("PDF 열기").clicked() {
                    self.open_dialog(ui.ctx());
                }
                if !self.file_name.is_empty() {
                    ui.weak(egui::RichText::new(&self.file_name).small());
                }

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    self.export_button(ui);
                });
            });
            if self.scanned < self.pages.len() {
                let progress = self.scanned as f32 / self.pages.len() as f32;
                ui.add(egui::ProgressBar::new(progress).desired_height(3.0));
            }
            ui.add_space(4.0);
        });
    }

    /// MS Office-style status bar: page count on the left, view switch on the right.
    fn bottom_bar(&mut self, ui: &mut egui::Ui) {
        if self.session.is_none() {
            return;
        }
        egui::Panel::bottom("status_bar").show_inside(ui, |ui| {
            ui.horizontal(|ui| {
                let n = self.pages.len();
                let label = if self.scanned < n {
                    format!("분석 중 {}/{}", self.scanned, n)
                } else {
                    format!("{} / {} 페이지", self.selected + 1, n)
                };
                ui.label(egui::RichText::new(label).small().weak());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.selectable_value(&mut self.overview, true, "오버뷰");
                    ui.selectable_value(&mut self.overview, false, "페이지");
                    if self.overview {
                        ui.add(
                            egui::Slider::new(&mut self.overview_zoom, 0.5..=5.0)
                                .logarithmic(true)
                                .show_value(false),
                        );
                        ui.weak(egui::RichText::new(format!("{:.0}%", self.overview_zoom * 100.0)).small());
                    }
                });
            });
        });
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn open_dialog(&mut self, _ctx: &egui::Context) {
        let Some(path) = rfd::FileDialog::new().add_filter("PDF", &["pdf"]).pick_file() else {
            return;
        };
        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "문서.pdf".into());
        match std::fs::read(&path) {
            Ok(bytes) => self.load(bytes, name),
            Err(e) => self.error = Some(format!("파일 읽기 실패: {e}")),
        }
    }

    #[cfg(target_arch = "wasm32")]
    fn open_dialog(&mut self, ctx: &egui::Context) {
        let (tx, ctx) = (self.inbox_tx.clone(), ctx.clone());
        wasm_bindgen_futures::spawn_local(async move {
            let Some(file) =
                rfd::AsyncFileDialog::new().add_filter("PDF", &["pdf"]).pick_file().await
            else {
                return;
            };
            let _ = tx.send(Inbox::Opened(file.file_name(), file.read().await));
            ctx.request_repaint();
        });
    }

    fn export_button(&mut self, ui: &mut egui::Ui) {
        let ready =
            self.session.is_some() && self.scanned == self.pages.len() && !self.pages.is_empty();
        if ui
            .add_enabled(ready, egui::Button::new(egui::RichText::new("크롭된 PDF 저장…").strong()))
            .clicked()
        {
            self.export(ui.ctx());
        }
    }

    fn export(&mut self, ctx: &egui::Context) {
        let Some(session) = self.session.as_ref() else { return };
        let lower = self.file_name.to_lowercase();
        let base = if let Some(stripped) = lower.strip_suffix(".pdf") {
            self.file_name[..stripped.len()].to_owned()
        } else {
            self.file_name.clone()
        };
        let suggested = if base.is_empty() {
            "cropped.pdf".to_owned()
        } else {
            format!("{base}_cropped.pdf")
        };
        #[cfg(not(target_arch = "wasm32"))]
        let Some(path) = rfd::FileDialog::new().set_file_name(suggested.clone()).save_file() else {
            return;
        };

        let dims: Vec<(f32, f32)> =
            (0..session.page_count()).map(|i| session.page_size(i).unwrap_or_default()).collect();
        let detected: Vec<Option<CropRect>> = self
            .pages
            .iter()
            .map(|p| match (&p.manual, &p.analysis) {
                (Some(m), _) => Some(*m),
                (None, Some(a)) if !a.blank => Some(a.crop),
                _ => None,
            })
            .collect();
        let enabled: Vec<bool> = self.pages.iter().map(|p| p.enabled).collect();
        let target = match self.settings.paper {
            None => TargetSize::Content,
            Some(paper) => {
                let (w, h) = paper.dims_pt();
                TargetSize::Paper { width: w, height: h }
            }
        };
        let options = PlanOptions {
            pad_mm: self.settings.pad_mm,
            mode: if self.settings.uniform { CropMode::Uniform } else { CropMode::PerPage },
            target,
            align: (self.settings.align.0 as f32 / 2.0, self.settings.align.1 as f32 / 2.0),
        };
        let placed = plan_crops(&dims, &detected, &enabled, &options);
        let bytes = match session.export(&placed) {
            Ok(bytes) => bytes,
            Err(e) => {
                self.error = Some(e);
                return;
            }
        };

        #[cfg(not(target_arch = "wasm32"))]
        {
            let _ = ctx;
            match std::fs::write(&path, bytes) {
                Ok(_) => {
                    // Open with the OS default app.
                    let opener = if cfg!(target_os = "macos") {
                        "open"
                    } else if cfg!(target_os = "windows") {
                        "explorer"
                    } else {
                        "xdg-open"
                    };
                    let _ = std::process::Command::new(opener).arg(&path).spawn();
                }
                Err(e) => self.error = Some(format!("쓰기 실패: {e}")),
            }
        }

        // Web: download the file and open it in a new tab. Both run inside the
        // click handler so the browser keeps the user gesture (popup allowed).
        #[cfg(target_arch = "wasm32")]
        {
            let _ = ctx;
            if let Err(e) = web_download_and_open(&bytes, &suggested) {
                self.error = Some(e);
            }
        }
    }

    /// Left: page list. Right: detection/output settings.
    fn sidebars(&mut self, ui: &mut egui::Ui) {
        if self.session.is_none() {
            return;
        }
        egui::Panel::left("pages_panel")
            .resizable(true)
            .default_size(CELL_WIDTH + 36.0)
            .show_inside(ui, |ui| {
                ui.add_space(4.0);
                ui.strong("페이지");
                ui.separator();
                let mut scroll = std::mem::take(&mut self.scroll_to_selected);
                egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                    let width = (ui.available_width() - 14.0).clamp(80.0, 320.0);
                    for i in 0..self.pages.len() {
                        let selected = i == self.selected && !self.overview;
                        let hit = draw_page(ui, &mut self.pages[i], i, width, selected);
                        if hit.clicked {
                            self.selected = i;
                            self.overview = false;
                        }
                        if selected && scroll {
                            ui.scroll_to_cursor(Some(egui::Align::Center));
                            scroll = false;
                        }
                        ui.add_space(6.0);
                    }
                });
            });

        egui::Panel::right("settings_panel")
            .resizable(true)
            .default_size(330.0)
            .show_inside(ui, |ui| {
                ui.add_space(4.0);
                ui.strong("옵션");
                ui.separator();
                egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                    self.settings_ui(ui);
                });
            });
    }

    /// Widget-gallery style: striped two-column grid, blue labels on the
    /// left, controls on the right; descriptions live in the label tooltip.
    /// Widget-gallery style: striped two-column grid, labels on the left,
    /// controls on the right; descriptions live in the label tooltip.
    fn settings_ui(&mut self, ui: &mut egui::Ui) {
        let row_label = |ui: &mut egui::Ui, text: &str, hint: &str| {
            ui.label(text).on_hover_text(hint);
        };
        // Fill the panel: label column + slider + value box + spacing.
        let slider_w = (ui.available_width() - 175.0).max(80.0);
        ui.spacing_mut().slider_width = slider_w;
        ui.spacing_mut().interact_size.y = 26.0;

        egui::Grid::new("settings_grid")
            .num_columns(2)
            .min_col_width(90.0)
            .striped(true)
            .spacing([14.0, 8.0])
            .show(ui, |ui| {
                row_label(ui, "감도", "높을수록 연한 회색도 잉크로 인식");
                ui.add(egui::Slider::new(&mut self.settings.threshold, 120..=250).integer());
                ui.end_row();

                row_label(ui, "여백 (mm)", "남기는 가장자리 여백");
                // Slider snaps to whole mm; the value box beside it accepts decimals.
                ui.horizontal(|ui| {
                    ui.add(
                        egui::Slider::new(&mut self.settings.pad_mm, 0.0..=20.0)
                            .step_by(1.0)
                            .clamping(egui::SliderClamping::Never)
                            .show_value(false),
                    );
                    let box_size = Vec2::new(ui.spacing().interact_size.x, ui.spacing().interact_size.y);
                    ui.add_sized(
                        box_size,
                        egui::DragValue::new(&mut self.settings.pad_mm)
                            .range(0.0..=f32::MAX)
                            .speed(0.1)
                            .custom_formatter(|v, _| {
                                if v.fract() == 0.0 { format!("{v:.0}") } else { format!("{v:.1}") }
                            }),
                    );
                });
                ui.end_row();

                row_label(ui, "노이즈", "한 줄에 이 이하 잉크점은 무시");
                ui.add(egui::Slider::new(&mut self.settings.min_ink, 1..=25).integer());
                ui.end_row();

                row_label(ui, "가장자리 무시", "스캔 가장자리의 얇은 선을 무시하는 폭 (mm)");
                ui.add(egui::Slider::new(&mut self.settings.edge_mm, 0.0..=10.0)
                        .step_by(0.5)
                        .fixed_decimals(1)
                        .clamping(egui::SliderClamping::Never));
                ui.end_row();

                row_label(ui, "자르기 기준", "모든 페이지를 같은 크기로 자름");
                ui.checkbox(&mut self.settings.uniform, "전체 공통");
                ui.end_row();

                row_label(ui, "출력 용지", "선택 시 내용을 용지에 여백을 두고 축소·중앙 배치");
                egui::ComboBox::from_id_salt("paper")
                    .width(slider_w + 60.0)
                    .selected_text(match self.settings.paper {
                        None => "자동".to_owned(),
                        Some(p) => p.label().to_owned(),
                    })
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut self.settings.paper, None, "자동 (크롭 크기)");
                        for p in [
                            PaperSize::A3,
                            PaperSize::A4,
                            PaperSize::A5,
                            PaperSize::B4Jis,
                            PaperSize::B5Jis,
                            PaperSize::Letter,
                            PaperSize::Legal,
                        ] {
                            let (w, h) = p.dims_mm();
                            ui.selectable_value(
                                &mut self.settings.paper,
                                Some(p),
                                format!("{} {:.0}×{:.0} mm", p.label(), w, h),
                            );
                        }
                    });
                ui.end_row();

                row_label(ui, "정렬", "용지 안에서 내용을 맞출 위치 (출력 용지 선택 시)");
                ui.add_enabled_ui(self.settings.paper.is_some(), |ui| {
                    anchor_picker(ui, &mut self.settings.align);
                });
                ui.end_row();
            });

        // Only threshold/noise need re-detection; mode/padding are pure
        // geometry applied when planning/exporting.
        let needs_reapply = self.scanned == self.pages.len()
            && !self.pages.is_empty()
            && self
                .applied
                .as_ref()
                .is_some_and(|a| a.threshold != self.settings.threshold
                    || a.min_ink != self.settings.min_ink
                    || a.edge_mm != self.settings.edge_mm);
        if needs_reapply {
            let now = ui.input(|i| i.time);
            let since = *self.settings_dirty_since.get_or_insert(now);
            let elapsed = std::time::Duration::from_secs_f64((now - since).max(0.0));
            if elapsed >= REDETECT_DEBOUNCE {
                self.settings_dirty_since = None;
                self.reapply();
            } else {
                ui.ctx().request_repaint_after(REDETECT_DEBOUNCE - elapsed);
            }
        } else {
            self.settings_dirty_since = None;
        }
    }

    fn center(&mut self, ui: &mut egui::Ui) {
        if self.session.is_none() {
            egui::CentralPanel::default().show_inside(ui, |ui| {
                ui.centered_and_justified(|ui| {
                    ui.vertical(|ui| {
                        ui.label(
                            egui::RichText::new("PDF 파일을 여기로 끌어다 놓으세요")
                                .heading()
                                .strong(),
                        );
                        ui.add_space(8.0);
                        ui.weak("또는 상단의 「PDF 열기」 버튼을 누르세요");
                    });
                });
            });
            return;
        }
        if self.overview {
            self.overview_ui(ui);
        } else {
            editor(self, ui);
        }
    }

    /// All pages as a grid; click selects, double-click opens in the editor.
    fn overview_ui(&mut self, ui: &mut egui::Ui) {
        egui::CentralPanel::default().show_inside(ui, |ui| {
            ui.weak(egui::RichText::new("클릭 = 선택 · 더블클릭 = 편집 · 체크 = 내보내기 포함").small());
            ui.add_space(2.0);
            // Pinch / ctrl+scroll resizes the thumbnails.
            let (zoom_delta, hovered) = ui.input(|i| {
                (i.zoom_delta(), i.pointer.hover_pos().is_some_and(|p| ui.max_rect().contains(p)))
            });
            if zoom_delta != 1.0 && hovered {
                self.overview_zoom = (self.overview_zoom * zoom_delta).clamp(0.5, 5.0);
            }
            let cell = CELL_WIDTH * self.overview_zoom;
            egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                let cols = ((ui.available_width() / (cell + 12.0)).floor() as usize).max(1);
                let mut index = 0;
                while index < self.pages.len() {
                    let row_end = (index + cols).min(self.pages.len());
                    ui.horizontal_top(|ui| {
                        for i in index..row_end {
                            let selected = i == self.selected;
                            let hit = draw_page(ui, &mut self.pages[i], i, cell, selected);
                            if hit.clicked {
                                self.selected = i;
                                self.scroll_to_selected = true;
                            }
                            if hit.double_clicked {
                                self.overview = false;
                            }
                        }
                    });
                    index = row_end;
                }
            });
        });
    }

    fn modal_windows(&mut self, ui: &mut egui::Ui) {
        let mut close_error = false;
        if let Some(err) = self.error.clone() {
            let mut open = true;
            egui::Window::new("오류")
                .open(&mut open)
                .collapsible(false)
                .show(ui.ctx(), |ui| {
                    ui.label(err);
                    ui.add_space(6.0);
                    if ui.button("닫기").clicked() {
                        close_error = true;
                    }
                });
            if !open || close_error {
                self.error = None;
            }
        }
    }
}

/// Center page editor: pinch/button zoom, scrolling, and manual crop editing.
fn editor(app: &mut App, ui: &mut egui::Ui) {
    if app.selected >= app.pages.len() {
        app.selected = 0;
    }
    let index = app.selected;
    let mut nav: Option<usize> = None;

    let total = app.pages.len();
    egui::CentralPanel::default().show_inside(ui, |ui| {
        {
            ui.horizontal(|ui| {
                if ui.button("이전").clicked() {
                    nav = Some((index + total - 1) % total);
                }
                ui.label(format!("{}/{}", index + 1, total));
                if ui.button("다음").clicked() {
                    nav = Some((index + 1) % total);
                }
                ui.separator();
                if ui.button("자동 검출").clicked() {
                    if let Some(page) = app.pages.get_mut(index) {
                        page.manual = None;
                    }
                }
                ui.separator();
                if ui.button("맞춤").clicked() {
                    app.preview_zoom = 1.0;
                }
                if ui.button("−").clicked() {
                    app.preview_zoom = (app.preview_zoom / 1.25).clamp(0.2, 8.0);
                }
                if ui.button("+").clicked() {
                    app.preview_zoom = (app.preview_zoom * 1.25).clamp(0.2, 8.0);
                }
                ui.weak(egui::RichText::new(format!("{:.0}%", app.preview_zoom * 100.0)).small());
                ui.separator();
                let mut enabled = app.pages.get(index).map(|p| p.enabled).unwrap_or(false);
                if ui.checkbox(&mut enabled, "내보내기 포함").changed() {
                    if let Some(page) = app.pages.get_mut(index) {
                        page.enabled = enabled;
                    }
                }
            });

            // Owned copies so the mutable closure below borrows nothing from
            // `app.pages`.
            let Some(page) = app.pages.get(index) else { return };
            let Some(tex) = page.texture.clone() else {
                ui.label("아직 분석되지 않은 페이지입니다.");
                return;
            };
            let Some(analysis) = page.analysis.clone() else { return };
            let manual = page.manual;
            // `page` borrow ends here; everything below uses owned clones.
            if manual.is_some() {
                ui.weak(egui::RichText::new("크롭 직접 조정됨").small());
            }
            let page_size = (analysis.page_width, analysis.page_height);
            let effective = manual.unwrap_or(analysis.crop);
            let blank = analysis.blank;
            let aspect = {
                let size = tex.size();
                size[1] as f32 / size[0] as f32
            };

            let base = {
                let avail = (ui.available_size() - Vec2::splat(12.0)).max(Vec2::splat(80.0));
                (avail.x.min(avail.y / aspect)).max(80.0)
            };
            let img_size = Vec2::new(base * app.preview_zoom, base * app.preview_zoom * aspect);

            // Sharp render: once the view rests, render the page at the on-screen
            // pixel size (bigger steps only; a larger texture is kept on zoom-out).
            let want_px = (img_size.x.max(img_size.y) * ui.pixels_per_point()).ceil() as u32;
            let tex = if want_px > MAX_PX + 100 {
                let step = DETAIL_STEPS.iter().copied().find(|&s| s >= want_px).unwrap_or(4000);
                let have = app.detail.as_ref().is_some_and(|d| d.index == index && d.px >= step);
                if !have {
                    let now = ui.input(|i| i.time);
                    match app.detail_wait {
                        Some((i, px, since)) if i == index && px == step => {
                            if now - since >= DETAIL_DEBOUNCE {
                                app.detail_wait = None;
                                if let Some(Ok(raster)) =
                                    app.session.as_ref().map(|s| s.render_detail(index, step))
                                {
                                    let texture = make_texture(ui.ctx(), 100_000 + index, &raster);
                                    app.detail = Some(DetailTex { index, px: step, texture });
                                }
                            } else {
                                ui.ctx().request_repaint_after(std::time::Duration::from_secs_f64(
                                    DETAIL_DEBOUNCE - (now - since),
                                ));
                            }
                        }
                        _ => {
                            app.detail_wait = Some((index, step, now));
                            ui.ctx().request_repaint_after(std::time::Duration::from_secs_f64(
                                DETAIL_DEBOUNCE,
                            ));
                        }
                    }
                }
                match &app.detail {
                    Some(d) if d.index == index => d.texture.clone(),
                    _ => tex,
                }
            } else {
                tex
            };

            let edge_mm = app.settings.edge_mm;
            let mut area = egui::ScrollArea::both().id_salt("preview_zoom_scroll");
            if let Some(offset) = app.pending_offset.take() {
                area = area.scroll_offset(offset);
            }
            let out = area
                .auto_shrink(false)
                .scroll_source(egui::scroll_area::ScrollSource {
                    drag: false,
                    ..Default::default()
                })
                .show(ui, |ui| {
                    let viewport = ui.available_size();
                    let host = Vec2::max(img_size + Vec2::splat(2.0 * HANDLE_PAD), viewport);
                    let (host_rect, _) = ui.allocate_exact_size(host, Sense::hover());
                    let rect =
                        egui::Rect::from_center_size(host_rect.center(), img_size.max(Vec2::splat(1.0)));
                    egui::Image::from_texture(egui::load::SizedTexture::new(tex.id(), rect.size()))
                        .paint_at(ui, rect);
                    paint_edge_band(&ui.painter_at(rect), rect, page_size, edge_mm);

                    if !blank {
                        // Overlay stays inside the image; handles use the full
                        // viewport painter so edge handles are not cut in half.
                        let painter = ui.painter_at(rect);
                        paint_crop_overlay(&painter, rect, page_size, &effective);
                        paint_handles(ui.painter(), rect, page_size, &effective);
                        edit_crop_interact(ui, rect, page_size, effective, index, app);
                    }
                });
            app.preview_offset = out.state.offset;
        }
    });

    if let Some(next) = nav {
        if next != index {
            app.selected = next;
            app.scroll_to_selected = true;
            app.crop_drag = None;
        }
    }
}

/// The crop box in preview-image pixels.
fn crop_box_px(image: egui::Rect, page: (f32, f32), crop: &CropRect) -> egui::Rect {
    let sx = image.width() / page.0;
    let sy = image.height() / page.1;
    egui::Rect::from_min_size(
        image.min + Vec2::new(crop.x * sx, crop.y * sy),
        Vec2::new(crop.width * sx, crop.height * sy),
    )
}

/// Which crop handle is under the pointer, if any.
fn handle_at(
    p: egui::Pos2,
    image: egui::Rect,
    page: (f32, f32),
    crop: &CropRect,
) -> Option<Handle> {
    let b = crop_box_px(image, page, crop);
    const TOL: f32 = 14.0;
    if p.x < b.min.x - TOL || p.x > b.max.x + TOL || p.y < b.min.y - TOL || p.y > b.max.y + TOL {
        return None;
    }
    let near_l = (p.x - b.min.x).abs() <= TOL;
    let near_r = (p.x - b.max.x).abs() <= TOL;
    let near_t = (p.y - b.min.y).abs() <= TOL;
    let near_b = (p.y - b.max.y).abs() <= TOL;
    Some(match (near_l, near_r, near_t, near_b) {
        (true, _, true, _) => Handle::TopLeft,
        (_, true, true, _) => Handle::TopRight,
        (true, _, _, true) => Handle::BottomLeft,
        (_, true, _, true) => Handle::BottomRight,
        (true, _, _, _) => Handle::Left,
        (_, true, _, _) => Handle::Right,
        (_, _, true, _) => Handle::Top,
        (_, _, _, true) => Handle::Bottom,
        _ => Handle::Inside,
    })
}

fn cursor_for(handle: Handle) -> egui::CursorIcon {
    match handle {
        Handle::Inside => egui::CursorIcon::Move,
        Handle::Left | Handle::Right => egui::CursorIcon::ResizeHorizontal,
        Handle::Top | Handle::Bottom => egui::CursorIcon::ResizeVertical,
        Handle::TopLeft | Handle::BottomRight => egui::CursorIcon::ResizeNwSe,
        Handle::TopRight | Handle::BottomLeft => egui::CursorIcon::ResizeNeSw,
    }
}

/// Apply a drag delta (in points) to the starting crop for a handle.
fn adjust_crop(start: CropRect, handle: Handle, dx: f32, dy: f32, page: (f32, f32)) -> CropRect {
    const MIN: f32 = 12.0;
    let mut x = start.x;
    let mut y = start.y;
    let mut w = start.width;
    let mut h = start.height;
    match handle {
        Handle::Inside => {
            x = start.x + dx;
            y = start.y + dy;
        }
        Handle::Left => {
            let nx = (start.x + dx).clamp(0.0, start.right() - MIN);
            w = start.right() - nx;
            x = nx;
        }
        Handle::Right => {
            let right = (start.right() + dx).clamp(start.x + MIN, page.0);
            w = right - start.x;
        }
        Handle::Top => {
            let ny = (start.y + dy).clamp(0.0, start.bottom() - MIN);
            h = start.bottom() - ny;
            y = ny;
        }
        Handle::Bottom => {
            let bottom = (start.bottom() + dy).clamp(start.y + MIN, page.1);
            h = bottom - start.y;
        }
        Handle::TopLeft => {
            x = (start.x + dx).clamp(0.0, start.right() - MIN);
            y = (start.y + dy).clamp(0.0, start.bottom() - MIN);
            w = start.right() - x;
            h = start.bottom() - y;
        }
        Handle::TopRight => {
            let right = (start.right() + dx).clamp(start.x + MIN, page.0);
            y = (start.y + dy).clamp(0.0, start.bottom() - MIN);
            w = right - start.x;
            h = start.bottom() - y;
        }
        Handle::BottomLeft => {
            x = (start.x + dx).clamp(0.0, start.right() - MIN);
            let bottom = (start.bottom() + dy).clamp(start.y + MIN, page.1);
            w = start.right() - x;
            h = bottom - start.y;
        }
        Handle::BottomRight => {
            let right = (start.right() + dx).clamp(start.x + MIN, page.0);
            let bottom = (start.bottom() + dy).clamp(start.y + MIN, page.1);
            w = right - start.x;
            h = bottom - start.y;
        }
    }
    let mut crop = CropRect::new(x, y, w, h);
    crop.x = crop.x.clamp(0.0, (page.0 - 2.0).max(0.0));
    crop.y = crop.y.clamp(0.0, (page.1 - 2.0).max(0.0));
    crop.width = crop.width.clamp(2.0, page.0 - crop.x);
    crop.height = crop.height.clamp(2.0, page.1 - crop.y);
    crop
}

/// Wire the drag interaction that edits the crop.
fn edit_crop_interact(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    page_size: (f32, f32),
    current: CropRect,
    index: usize,
    app: &mut App,
) {
    let response =
        ui.interact(
        rect.expand(HANDLE_PAD),
        egui::Id::new(("crop_edit", index)),
        Sense::click_and_drag(),
    );
    let px_per_pt = (rect.width() / page_size.0, rect.height() / page_size.1);
    let pointer = response.interact_pointer_pos();

    // Pinch-to-zoom while the pointer is over the page image.
    // zoom_delta() is a multiplicative factor (1.0 = no change).
    // Like ayagami-demo: multiply by zoom_delta, keep the point under the
    // cursor fixed.
    let (zoom_delta, hover) = ui.ctx().input(|i| (i.zoom_delta(), i.pointer.hover_pos()));
    if zoom_delta != 1.0 {
        if let Some(p) = hover.filter(|p| rect.contains(*p)) {
            let old = app.preview_zoom;
            let new = (old * zoom_delta).clamp(0.2, 8.0);
            if new != old {
                let ratio = new / old;
                let rel = p - rect.min;
                app.pending_offset =
                    Some((app.preview_offset + rel * (ratio - 1.0)).max(Vec2::ZERO));
                app.preview_zoom = new;
            }
        }
    }

    if !response.dragged() {
        if let Some(handle) = pointer.and_then(|p| handle_at(p, rect, page_size, &current)) {
            ui.ctx().set_cursor_icon(cursor_for(handle));
        }
    }

    if response.drag_started() {
        if let Some(p) = pointer {
            if let Some(handle) = handle_at(p, rect, page_size, &current) {
                app.crop_drag = Some(CropDrag {
                    handle,
                    index,
                    start_px: Vec2::new(p.x - rect.min.x, p.y - rect.min.y),
                    start_crop: current,
                });
            }
        }
    }

    let drag = app.crop_drag;
    if let Some(drag) = drag {
        if drag.index != index {
            return;
        }
        if response.drag_stopped() {
            app.crop_drag = None;
        } else if response.dragged() {
            if let Some(p) = pointer {
                let dx = (p.x - rect.min.x - drag.start_px.x) / px_per_pt.0;
                let dy = (p.y - rect.min.y - drag.start_px.y) / px_per_pt.1;
                let crop = adjust_crop(drag.start_crop, drag.handle, dx, dy, page_size);
                if let Some(page) = app.pages.get_mut(index) {
                    page.manual = Some(crop);
                }
            }
        }
    }
}

/// Resize handles around the crop box.
fn paint_handles(painter: &egui::Painter, image: egui::Rect, page: (f32, f32), crop: &CropRect) {
    let b = crop_box_px(image, page, crop);
    let xs = [b.min.x, (b.min.x + b.max.x) / 2.0, b.max.x];
    let ys = [b.min.y, (b.min.y + b.max.y) / 2.0, b.max.y];
    for (i, &x) in xs.iter().enumerate() {
        for (j, &y) in ys.iter().enumerate() {
            if i == 1 && j == 1 {
                continue;
            }
            let handle_rect = egui::Rect::from_center_size(egui::pos2(x, y), Vec2::splat(HANDLE_SIZE));
            painter.rect_filled(handle_rect, 1.5, Color32::WHITE);
            painter.rect_stroke(
                handle_rect,
                CornerRadius::ZERO,
                Stroke::new(1.5, Color32::from_rgb(255, 93, 93)),
                StrokeKind::Inside,
            );
        }
    }
}

fn make_texture(
    ctx: &egui::Context,
    index: usize,
    raster: &cropper_core::render::PageRaster,
) -> egui::TextureHandle {
    let mut rgba = Vec::with_capacity(raster.luma.len() * 4);
    for &g in &raster.luma {
        rgba.extend_from_slice(&[g, g, g, 255]);
    }
    let image = egui::ColorImage::from_rgba_unmultiplied(
        [raster.width as usize, raster.height as usize],
        &rgba,
    );
    ctx.load_texture(format!("page-{index}"), image, egui::TextureOptions::default())
}

/// 3×3 anchor picker; paints its own marks so it needs no icon font.
fn anchor_picker(ui: &mut egui::Ui, align: &mut (u8, u8)) {
    const CELL: f32 = 22.0;
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(CELL * 3.0), Sense::hover());
    for v in 0..3u8 {
        for h in 0..3u8 {
            let cell = egui::Rect::from_min_size(
                rect.min + Vec2::new(h as f32 * CELL, v as f32 * CELL),
                Vec2::splat(CELL),
            )
            .shrink(1.5);
            let response = ui.interact(cell, ui.id().with(("anchor", h, v)), Sense::click());
            let selected = *align == (h, v);
            let visuals = ui.style().interact_selectable(&response, selected);
            ui.painter().rect_filled(cell, 3.0, visuals.weak_bg_fill);
            if selected {
                ui.painter().circle_filled(cell.center(), 4.0, visuals.fg_stroke.color);
            }
            if response.clicked() {
                *align = (h, v);
            }
        }
    }
}

/// Tint the page-edge band the detector ignores (`edge_mm` on every side).
fn paint_edge_band(painter: &egui::Painter, image: egui::Rect, page: (f32, f32), edge_mm: f32) {
    if edge_mm <= 0.0 {
        return;
    }
    let px = (edge_mm / MM_PER_PT * image.width() / page.0)
        .min(image.width().min(image.height()) / 4.0);
    let inner = image.shrink(px);
    let tint = Color32::from_rgba_unmultiplied(255, 170, 40, 70);
    painter.rect_filled(image.top_strip(inner), 0.0, tint);
    painter.rect_filled(image.bottom_strip(inner), 0.0, tint);
    painter.rect_filled(image.left_strip(inner), 0.0, tint);
    painter.rect_filled(image.right_strip(inner), 0.0, tint);
    painter.rect_stroke(
        inner,
        CornerRadius::ZERO,
        Stroke::new(1.0, Color32::from_rgb(255, 170, 40)),
        StrokeKind::Inside,
    );
}

/// Dim everything outside the crop and stroke the crop box.
fn paint_crop_overlay(
    painter: &egui::Painter,
    image: egui::Rect,
    page: (f32, f32),
    crop: &CropRect,
) {
    let b = crop_box_px(image, page, crop);
    let blank = crop.width <= 0.0;
    if blank {
        return;
    }
    let dim = Color32::from_rgba_unmultiplied(8, 10, 20, 130);
    painter.rect_filled(image.top_strip(b), 0.0, dim);
    painter.rect_filled(image.bottom_strip(b), 0.0, dim);
    painter.rect_filled(image.left_strip(b), 0.0, dim);
    painter.rect_filled(image.right_strip(b), 0.0, dim);
    painter.rect_stroke(
        b,
        CornerRadius::ZERO,
        Stroke::new(1.5, Color32::from_rgb(255, 93, 93)),
        StrokeKind::Inside,
    );
}

/// One page thumbnail: image + crop overlay + caption; click toggles include,
/// double-click opens the big preview (returns true on double-click).
struct PageHit {
    clicked: bool,
    double_clicked: bool,
}

fn draw_page(
    ui: &mut egui::Ui,
    page: &mut PageData,
    index: usize,
    width: f32,
    selected: bool,
) -> PageHit {
    let analysis = page.analysis.clone();
    let aspect = match (&page.texture, &analysis) {
        (Some(tex), Some(_)) => {
            let size = tex.size();
            size[1] as f32 / size[0] as f32
        }
        _ => 1.4,
    };
    let img_size = Vec2::new(width, width * aspect);

    let mut hit = PageHit { clicked: false, double_clicked: false };
    ui.vertical(|ui| {
        let (rect, _) = ui.allocate_exact_size(img_size, Sense::hover());
        let painter = ui.painter_at(rect);

        match (&page.texture, &analysis) {
            (Some(tex), Some(a)) => {
                egui::Image::from_texture(egui::load::SizedTexture::new(tex.id(), img_size))
                    .paint_at(ui, rect);
                let effective = page.manual.unwrap_or(a.crop);
                paint_crop_overlay(
                    &painter,
                    rect,
                    (a.page_width, a.page_height),
                    &effective,
                );
            }
            _ => {
                painter.rect_filled(rect, 4.0, Color32::from_rgb(24, 27, 36));
                painter.text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    format!("{}…", index + 1),
                    egui::FontId::proportional(14.0),
                    Color32::GRAY,
                );
            }
        }

        let response = ui.interact(rect, egui::Id::new(("page", index)), Sense::click());
        if analysis.is_some() {
            hit.clicked = response.clicked();
            hit.double_clicked = response.double_clicked();
        }
        if selected {
            painter.rect_stroke(
                rect,
                CornerRadius::ZERO,
                Stroke::new(2.5, Color32::from_rgb(90, 150, 255)),
                StrokeKind::Inside,
            );
        }

        // Caption row; manual crop wins over the detected one.
        let caption = match &analysis {
            None => format!("{} · 분석 중", index + 1),
            Some(a) if a.blank => format!("{} · 빈 페이지", index + 1),
            Some(a) if !page.enabled => format!("{} · 제외됨", index + 1),
            Some(a) => {
                let crop = page.manual.unwrap_or(a.crop);
                let tag = if page.manual.is_some() { " (수정)" } else { "" };
                format!(
                    "{} · L{:.0} T{:.0} R{:.0} B{:.0} mm{}",
                    index + 1,
                    crop.x * MM_PER_PT,
                    crop.y * MM_PER_PT,
                    (a.page_width - crop.right()) * MM_PER_PT,
                    (a.page_height - crop.bottom()) * MM_PER_PT,
                    tag
                )
            }
        };
        ui.horizontal(|ui| {
            ui.checkbox(&mut page.enabled, "");
            ui.label(
                egui::RichText::new(caption).small().color(if page.enabled {
                    Color32::from_rgb(175, 180, 195)
                } else {
                    Color32::GRAY
                }),
            );
        });

        if !page.enabled && page.texture.is_some() {
            painter.rect_filled(rect, 0.0, Color32::from_rgba_unmultiplied(30, 30, 36, 150));
        }
    });
    hit
}

trait RectStrips {
    fn top_strip(&self, inner: egui::Rect) -> egui::Rect;
    fn bottom_strip(&self, inner: egui::Rect) -> egui::Rect;
    fn left_strip(&self, inner: egui::Rect) -> egui::Rect;
    fn right_strip(&self, inner: egui::Rect) -> egui::Rect;
}

impl RectStrips for egui::Rect {
    fn top_strip(&self, inner: egui::Rect) -> egui::Rect {
        egui::Rect::from_min_max(self.min, egui::pos2(self.max.x, inner.top()))
    }
    fn bottom_strip(&self, inner: egui::Rect) -> egui::Rect {
        egui::Rect::from_min_max(egui::pos2(self.min.x, inner.bottom()), self.max)
    }
    fn left_strip(&self, inner: egui::Rect) -> egui::Rect {
        egui::Rect::from_min_max(egui::pos2(self.min.x, inner.top()), egui::pos2(inner.left(), inner.bottom()))
    }
    fn right_strip(&self, inner: egui::Rect) -> egui::Rect {
        egui::Rect::from_min_max(
            egui::pos2(inner.right(), inner.top()),
            egui::pos2(self.max.x, inner.bottom()),
        )
    }
}

fn install_fonts(ctx: &egui::Context, hangul: Vec<u8>) {
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        "pretendard".into(),
        egui::FontData::from_owned(hangul).into(),
    );
    // egui's bundled fonts are compiled out (see Cargo.toml), so Pretendard
    // is the only font.
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        fonts.families.entry(family).or_default().insert(0, "pretendard".into());
    }
    ctx.set_fonts(fonts);
}

#[cfg(not(target_arch = "wasm32"))]
fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 880.0])
            .with_min_inner_size([900.0, 600.0])
            .with_title("Cropper — 스캔 PDF 여백 자르기"),
        ..Default::default()
    };
    eframe::run_native(
        "cropper",
        options,
        Box::new(|cc| {
            install_fonts(
                &cc.egui_ctx,
                include_bytes!("../assets/PretendardVariable.ttf").to_vec(),
            );
            let mut app = App::new();
            // Handy for demos/tests: CROPPER_SAMPLE=path/to.pdf
            if let Ok(path) = std::env::var("CROPPER_SAMPLE") {
                if let Ok(bytes) = std::fs::read(&path) {
                    app.load(bytes, "sample.pdf".into());
                }
            }
            Ok(Box::new(app))
        }),
    )
}

/// Web entry: `index.html` must contain `<canvas id="cropper_canvas">`.
#[cfg(target_arch = "wasm32")]
fn main() {
    use eframe::wasm_bindgen::JsCast as _;
    console_error_panic_hook::set_once();
    wasm_bindgen_futures::spawn_local(async {
        let canvas = web_sys::window()
            .and_then(|w| w.document())
            .and_then(|d| d.get_element_by_id("cropper_canvas"))
            .and_then(|e| e.dyn_into::<web_sys::HtmlCanvasElement>().ok())
            .expect("canvas #cropper_canvas not found");
        // Hangul font is fetched at runtime instead of being baked into the wasm.
        let font = fetch_bytes(WEB_FONT_URL).await;
        let result = eframe::WebRunner::new()
            .start(
                canvas,
                eframe::WebOptions::default(),
                Box::new(|cc| {
                    let font = font.ok_or("폰트를 불러오지 못했습니다")?;
                    install_fonts(&cc.egui_ctx, font);
                    let mut app = App::new();
                    if let Some(query) = web_sys::window().and_then(|w| w.location().search().ok()) {
                        app.apply_query(&query);
                    }
                    Ok(Box::new(app))
                }),
            )
            .await;
        if let Some(loading) = web_sys::window()
            .and_then(|w| w.document())
            .and_then(|d| d.get_element_by_id("loading"))
        {
            match result {
                Ok(_) => loading.remove(),
                Err(e) => loading.set_inner_html(&format!("시작 실패: {e:?}")),
            }
        }
    });
}

#[cfg(target_arch = "wasm32")]
const WEB_FONT_URL: &str = "https://cdn.jsdelivr.net/gh/orioncactus/pretendard@v1.3.9/packages/pretendard/dist/public/static/alternative/Pretendard-Regular.ttf";

/// GET `url` and return the body; `None` on any network/HTTP failure.
#[cfg(target_arch = "wasm32")]
async fn fetch_bytes(url: &str) -> Option<Vec<u8>> {
    use eframe::wasm_bindgen::JsCast as _;
    use wasm_bindgen_futures::JsFuture;
    let window = web_sys::window()?;
    let response: web_sys::Response =
        JsFuture::from(window.fetch_with_str(url)).await.ok()?.dyn_into().ok()?;
    if !response.ok() {
        return None;
    }
    let buffer = JsFuture::from(response.array_buffer().ok()?).await.ok()?;
    Some(js_sys::Uint8Array::new(&buffer).to_vec())
}

/// Save `bytes` as `name` (download) and open the same blob in a new tab.
#[cfg(target_arch = "wasm32")]
fn web_download_and_open(bytes: &[u8], name: &str) -> Result<(), String> {
    use eframe::wasm_bindgen::JsCast as _;
    let js_err = |e: eframe::wasm_bindgen::JsValue| format!("{e:?}");

    let array = js_sys::Uint8Array::from(bytes);
    let parts = js_sys::Array::of1(&array);
    let opts = web_sys::BlobPropertyBag::new();
    opts.set_type("application/pdf");
    let blob = web_sys::Blob::new_with_u8_array_sequence_and_options(&parts, &opts).map_err(js_err)?;
    let url = web_sys::Url::create_object_url_with_blob(&blob).map_err(js_err)?;

    let window = web_sys::window().ok_or("window 없음")?;
    let document = window.document().ok_or("document 없음")?;
    let anchor: web_sys::HtmlAnchorElement =
        document.create_element("a").map_err(js_err)?.dyn_into().map_err(|_| "a 요소 생성 실패")?;
    anchor.set_href(&url);
    anchor.set_download(name);
    anchor.click();

    // Firefox already opens downloaded PDFs itself, so skip the extra tab there.
    // Elsewhere the popup may be blocked; the download above still happened.
    let firefox = window.navigator().user_agent().is_ok_and(|ua| ua.contains("Firefox"));
    if !firefox {
        // Open the tab now (inside the click) and let JS point it at a readable
        // service-worker URL once the PDF is cached; blob URL is the fallback.
        let tab = window.open_with_url_and_target("about:blank", "_blank").ok().flatten();
        let helper = js_sys::Reflect::get(&window, &"cropperOpenPdf".into())
            .ok()
            .and_then(|f| f.dyn_into::<js_sys::Function>().ok());
        match (helper, &tab) {
            (Some(helper), _) => {
                let tab = tab.map(eframe::wasm_bindgen::JsValue::from).unwrap_or_default();
                let _ = helper.call4(&window, &array, &name.into(), &tab, &url.as_str().into());
            }
            (None, Some(tab)) => {
                let _ = tab.location().set_href(&url);
            }
            _ => {}
        }
    }
    Ok(())
}
