//! The loop editor window.
//!
//! Two frame viewers (Start and End) can be scrubbed frame by frame; frames
//! are decoded on a background thread, latest request wins, so dragging a
//! slider never queues up work. The live "match" compares the two frames and
//! their neighbours exactly like the loop search does. A square crop can be
//! dragged on the frames and is previewed as the pump would show it.
//!
//! Non-ASCII characters are written as escapes so no tool's code page can
//! mangle them.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};

use aio_loop::crop::{self, Crop};
use aio_loop::ffmpeg::{self, CutOptions, Tools, VideoInfo};
use aio_loop::matcher::{self, CONTEXT, Thumb, quality};
use aio_loop::{MAX_RADIUS, Search, Stage, search, timecode};
use eframe::egui;
use egui::{Color32, Pos2, Rect, RichText, Sense, Stroke, TextureHandle, TextureOptions, Vec2};

const ELLIPSIS: &str = "\u{2026}";
const TIMES: &str = "\u{d7}";
const RED: Color32 = Color32::from_rgb(0xE0, 0x6C, 0x5C);
const GREEN: Color32 = Color32::from_rgb(0x4C, 0xC3, 0x6E);
const ACCENT: Color32 = Color32::from_rgb(0x2E, 0xC4, 0xB6);
const VIDEO_EXTS: &[&str] = &["mp4", "m4v", "mkv", "webm", "mov", "avi", "wmv", "gif"];
/// Width the viewer frames are decoded at.
const VIEW_WIDTH: u32 = 480;
/// The pump display's resolution, for "scale to display size".
const DISPLAY_SIZE: u32 = 480;

fn color_for(score: f32) -> Color32 {
    match quality(score) {
        "excellent" | "good" => GREEN,
        "fair" => Color32::from_rgb(0xE8, 0xB3, 0x4B),
        _ => RED,
    }
}

/// "99.12% match" from a 0..1 difference score.
fn match_text(score: f32) -> String {
    format!("{:.2}% match ({})", (1.0 - score) * 100.0, quality(score))
}

// ---------------------------------------------------------------------------
// Background frame decoding

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Slot {
    Start,
    End,
}

/// Decoded frames around one frame: `frames[i]` is frame `first + i`.
struct Window {
    center: u64,
    first: u64,
    width: u32,
    height: u32,
    frames: Vec<Vec<u8>>,
    thumbs: Vec<Thumb>,
}

impl Window {
    fn center_index(&self) -> usize {
        (self.center - self.first) as usize
    }

    /// Frame `n` if this window decoded it (the center or a neighbour).
    fn frame(&self, n: u64) -> Option<&[u8]> {
        n.checked_sub(self.first).and_then(|i| self.frames.get(i as usize)).map(Vec::as_slice)
    }
}

/// Decodes windows around requested frames, one request at a time; when
/// several arrive while it is busy, only the latest per viewer is decoded.
struct Fetcher {
    tx: Sender<(Slot, u64)>,
    done: Arc<Mutex<HashMap<Slot, Result<Window, String>>>>,
}

impl Fetcher {
    fn start(tools: Arc<Tools>, info: VideoInfo, ctx: egui::Context) -> Self {
        let (tx, rx) = mpsc::channel::<(Slot, u64)>();
        let done = Arc::new(Mutex::new(HashMap::new()));
        let results = done.clone();
        std::thread::spawn(move || decode_loop(&tools, &info, &ctx, &rx, &results));
        Self { tx, done }
    }

    fn request(&self, slot: Slot, frame: u64) {
        let _ = self.tx.send((slot, frame));
    }

    fn take(&self, slot: Slot) -> Option<Result<Window, String>> {
        self.done.lock().unwrap().remove(&slot)
    }
}

fn decode_loop(
    tools: &Tools,
    info: &VideoInfo,
    ctx: &egui::Context,
    rx: &Receiver<(Slot, u64)>,
    results: &Mutex<HashMap<Slot, Result<Window, String>>>,
) {
    let last = info.frames.saturating_sub(1);
    let height = info.preview_height(VIEW_WIDTH);
    while let Ok(first_request) = rx.recv() {
        let mut latest = HashMap::from([first_request]);
        latest.extend(rx.try_iter()); // drop superseded requests
        for (slot, center) in latest {
            let center = center.min(last);
            let first = center.saturating_sub(CONTEXT as u64);
            let count = (center + CONTEXT as u64).min(last) - first + 1;
            let window = ffmpeg::decode_frames(tools, info, first, count, VIEW_WIDTH)
                .map(|frames| Window {
                    center,
                    first,
                    width: VIEW_WIDTH,
                    height,
                    thumbs: frames.iter().map(|f| Thumb::from_rgb(f, VIEW_WIDTH as usize, height as usize)).collect(),
                    frames,
                })
                .map_err(|e| format!("{e:#}"))
                .and_then(|w| if w.center_index() < w.frames.len() { Ok(w) } else { Err("frame not decoded".into()) });
            results.lock().unwrap().insert(slot, window);
            ctx.request_repaint();
        }
    }
}

// ---------------------------------------------------------------------------
// Jobs (probe, search, save, show on display)

enum Done {
    Info(Result<VideoInfo, String>),
    Search(Result<Search, String>),
    Saved(Result<PathBuf, String>),
    Shown(Result<(), String>),
}

#[derive(Default)]
struct Job {
    running: bool,
    stage: String,
    progress: Option<f32>,
    done: Option<Done>,
}

/// One frame viewer.
struct Viewer {
    frame: u64,
    window: Option<Window>,
    texture: Option<(u64, TextureHandle)>,
    error: Option<String>,
}

impl Viewer {
    fn new(frame: u64) -> Self {
        Self { frame, window: None, texture: None, error: None }
    }

    /// True while the selected frame is not on screen yet. A neighbour of the
    /// last decoded frame (one or two steps away) shows immediately.
    fn loading(&self) -> bool {
        self.window.as_ref().is_none_or(|w| w.frame(self.frame).is_none()) && self.error.is_none()
    }
}

struct LoopApp {
    tools: Result<Arc<Tools>, String>,
    job: Arc<Mutex<Job>>,
    /// Started from AIO Display: crop on by default, scaled for the pump.
    for_display: bool,
    info: Option<VideoInfo>,
    fetcher: Option<Fetcher>,
    start: Viewer,
    end: Viewer,
    crop_on: bool,
    crop: Crop,
    radius: f64,
    search: Option<Search>,
    output: String,
    audio: bool,
    scale_to_display: bool,
    saved: Option<PathBuf>,
    message: Option<(bool, String)>,
}

impl LoopApp {
    fn new(for_display: bool) -> Self {
        Self {
            tools: ffmpeg::find_tools().map(Arc::new).map_err(|e| format!("{e:#}")),
            job: Arc::new(Mutex::new(Job::default())),
            for_display,
            info: None,
            fetcher: None,
            start: Viewer::new(0),
            end: Viewer::new(0),
            crop_on: for_display,
            crop: Crop { x: 0, y: 0, size: 0 },
            radius: 2.0,
            search: None,
            output: String::new(),
            audio: false,
            scale_to_display: true,
            saved: None,
            message: None,
        }
    }

    fn busy(&self) -> bool {
        self.job.lock().unwrap().running
    }

    /// Runs `work` on a thread; its result arrives in `Job::done`.
    fn spawn(&self, ctx: &egui::Context, stage: &str, work: impl FnOnce(&Mutex<Job>, &egui::Context) -> Done + Send + 'static) {
        *self.job.lock().unwrap() = Job { running: true, stage: stage.to_owned(), progress: None, done: None };
        let (job, ctx) = (self.job.clone(), ctx.clone());
        std::thread::spawn(move || {
            let done = work(&job, &ctx);
            let mut j = job.lock().unwrap();
            j.running = false;
            j.done = Some(done);
            drop(j);
            ctx.request_repaint();
        });
    }

    fn open(&mut self, ctx: &egui::Context, path: PathBuf) {
        let Ok(tools) = self.tools.clone() else { return };
        self.message = None;
        self.spawn(ctx, &format!("Reading video{ELLIPSIS}"), move |_, _| {
            Done::Info(ffmpeg::probe(&tools, &path).map_err(|e| format!("{e:#}")))
        });
    }

    fn set_frame(&mut self, slot: Slot, frame: u64) {
        let Some(info) = &self.info else { return };
        let last = info.frames.saturating_sub(1);
        let viewer = match slot {
            Slot::Start => &mut self.start,
            Slot::End => &mut self.end,
        };
        viewer.frame = frame.min(last);
        viewer.error = None;
        if viewer.window.as_ref().is_none_or(|w| w.center != viewer.frame)
            && let Some(f) = &self.fetcher
        {
            f.request(slot, viewer.frame);
        }
    }

    fn find(&mut self, ctx: &egui::Context) {
        let (Ok(tools), Some(info)) = (self.tools.clone(), self.info.clone()) else { return };
        let (start, end) = (info.rate.time_of(self.start.frame), info.rate.time_of(self.end.frame));
        let radius = self.radius;
        self.message = None;
        self.spawn(ctx, &format!("Decoding frames{ELLIPSIS}"), move |job, ctx| {
            let result = search(&tools, &info, start, end, radius, |stage| {
                job.lock().unwrap().stage = match stage {
                    Stage::DecodingStart => format!("Decoding frames around the start{ELLIPSIS}"),
                    Stage::DecodingEnd => format!("Decoding frames around the end{ELLIPSIS}"),
                    Stage::Comparing => format!("Comparing frames{ELLIPSIS}"),
                };
                ctx.request_repaint();
            });
            Done::Search(result.map_err(|e| format!("{e:#}")))
        });
    }

    fn save(&mut self, ctx: &egui::Context) {
        let (Ok(tools), Some(info)) = (self.tools.clone(), self.info.clone()) else { return };
        let opts = CutOptions {
            start: self.start.frame,
            end: self.end.frame,
            crop: self.crop_on.then_some(self.crop),
            scale: (self.crop_on && self.scale_to_display).then_some(DISPLAY_SIZE),
            audio: self.audio,
        };
        let output = PathBuf::from(self.output.trim());
        self.saved = None;
        self.message = None;
        self.spawn(ctx, &format!("Saving loop{ELLIPSIS}"), move |job, ctx| {
            let result = ffmpeg::cut(&tools, &info, &opts, &output, |p| {
                job.lock().unwrap().progress = Some(p);
                ctx.request_repaint();
            });
            Done::Saved(result.map(|()| output).map_err(|e| format!("{e:#}")))
        });
    }

    fn show_on_display(&mut self, ctx: &egui::Context, path: PathBuf) {
        self.spawn(ctx, &format!("Importing on the pump display{ELLIPSIS}"), move |_, _| {
            let result = (|| {
                let path = std::path::absolute(&path).map_err(|e| e.to_string())?;
                let mut client = aio_ipc::Client::connect().map_err(|e| format!("daemon: {e}"))?;
                match client.request(&aio_ipc::Request::SetSource { source: aio_ipc::Source::Video { path } }) {
                    Ok(aio_ipc::Response::Ok) => Ok(()),
                    Ok(aio_ipc::Response::Error { message }) => Err(message),
                    Ok(other) => Err(format!("unexpected answer {other:?}")),
                    Err(e) => Err(e.to_string()),
                }
            })();
            Done::Shown(result)
        });
    }

    fn finish_job(&mut self, ctx: &egui::Context) {
        let Some(done) = self.job.lock().unwrap().done.take() else { return };
        match done {
            Done::Info(Ok(info)) => {
                let last = info.frames.saturating_sub(1);
                self.output = ffmpeg::loop_path(&info.path).display().to_string();
                self.audio = false;
                self.search = None;
                self.saved = None;
                self.crop = Crop::centered_max(info.width, info.height);
                self.fetcher = self.tools.clone().ok().map(|t| Fetcher::start(t, info.clone(), ctx.clone()));
                self.start = Viewer::new(info.rate.frame_at(1.0).min(last / 2));
                self.end = Viewer::new(last);
                let (s, e) = (self.start.frame, self.end.frame);
                self.info = Some(info);
                self.set_frame(Slot::Start, s);
                self.set_frame(Slot::End, e);
            }
            Done::Search(Ok(search)) => {
                let best = search.candidates[0];
                self.search = Some(search);
                self.set_frame(Slot::Start, best.start);
                self.set_frame(Slot::End, best.end);
            }
            Done::Saved(Ok(path)) => self.saved = Some(path),
            Done::Shown(Ok(())) => self.message = Some((true, "Now playing on the pump display.".into())),
            Done::Info(Err(e)) | Done::Search(Err(e)) | Done::Saved(Err(e)) | Done::Shown(Err(e)) => {
                self.message = Some((false, e));
            }
        }
    }

    /// Collects decoded windows and turns new center frames into textures.
    fn poll_frames(&mut self, ctx: &egui::Context) {
        let Some(fetcher) = &self.fetcher else { return };
        for (slot, viewer) in [(Slot::Start, &mut self.start), (Slot::End, &mut self.end)] {
            match fetcher.take(slot) {
                Some(Ok(w)) => viewer.window = Some(w),
                Some(Err(e)) => viewer.error = Some(e),
                None => {}
            }
            let Some(w) = &viewer.window else { continue };
            // Show the selected frame if it is already decoded (a neighbour),
            // else the last decoded one until the new window arrives.
            let shown = if w.frame(viewer.frame).is_some() { viewer.frame } else { w.center };
            if viewer.texture.as_ref().is_none_or(|(n, _)| *n != shown) {
                let rgb = w.frame(shown).expect("shown frame is in the window");
                let image = egui::ColorImage::from_rgb([w.width as usize, w.height as usize], rgb);
                match &mut viewer.texture {
                    Some((n, tex)) => {
                        tex.set(image, TextureOptions::LINEAR);
                        *n = shown;
                    }
                    None => viewer.texture = Some((shown, ctx.load_texture(format!("{slot:?}"), image, TextureOptions::LINEAR))),
                }
            }
        }
    }

    /// The live score of the current Start/End pair, once both are decoded.
    fn live_score(&self) -> Option<f32> {
        let (a, b) = (self.start.window.as_ref()?, self.end.window.as_ref()?);
        if a.center != self.start.frame || b.center != self.end.frame {
            return None;
        }
        matcher::aligned_score(&a.thumbs, a.center_index(), &b.thumbs, b.center_index())
    }

    fn info_ui(ui: &mut egui::Ui, info: &VideoInfo) {
        let name = info.path.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        ui.label(RichText::new(name).strong());
        ui.weak(format!(
            "{}{TIMES}{}  |  {:.3} fps  |  {}  |  {} frames  |  {}{}",
            info.width,
            info.height,
            info.rate.fps(),
            timecode::format(info.duration),
            info.frames,
            info.codec,
            if info.has_audio { "  |  audio" } else { "" }
        ));
    }

    /// Draws one viewer: frame, crop overlay, slider and frame steps.
    fn viewer_ui(&mut self, ui: &mut egui::Ui, slot: Slot, title: &str, hint: &str, width: f32, info: &VideoInfo) {
        let size = Vec2::new(width, width * info.height as f32 / info.width.max(1) as f32);
        let rate = info.rate;
        let last = info.frames.saturating_sub(1);
        ui.vertical(|ui| {
            ui.set_width(width);
            ui.horizontal(|ui| {
                ui.label(RichText::new(title).strong());
                ui.weak(hint);
            });
            let (rect, response) = ui.allocate_exact_size(size, Sense::click_and_drag());
            let painter = ui.painter_at(rect);
            let viewer = match slot {
                Slot::Start => &self.start,
                Slot::End => &self.end,
            };
            match &viewer.texture {
                Some((_, tex)) => painter.image(tex.id(), rect, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE),
                None => painter.rect_filled(rect, 0.0, Color32::from_gray(20)),
            };
            if viewer.loading() {
                painter.text(rect.left_top() + Vec2::splat(6.0), egui::Align2::LEFT_TOP, format!("decoding{ELLIPSIS}"), egui::FontId::proportional(13.0), Color32::WHITE);
            }
            if let Some(e) = &viewer.error {
                painter.text(rect.center(), egui::Align2::CENTER_CENTER, e, egui::FontId::proportional(12.0), RED);
            }

            if self.crop_on {
                let to_screen = |x: f64, y: f64| {
                    Pos2::new(
                        rect.left() + (x / f64::from(info.width)) as f32 * rect.width(),
                        rect.top() + (y / f64::from(info.height)) as f32 * rect.height(),
                    )
                };
                let c = self.crop;
                let r = Rect::from_min_max(
                    to_screen(f64::from(c.x), f64::from(c.y)),
                    to_screen(f64::from(c.x + c.size), f64::from(c.y + c.size)),
                );
                let dim = Color32::from_black_alpha(150);
                painter.rect_filled(Rect::from_min_max(rect.left_top(), Pos2::new(rect.right(), r.top())), 0.0, dim);
                painter.rect_filled(Rect::from_min_max(Pos2::new(rect.left(), r.bottom()), rect.right_bottom()), 0.0, dim);
                painter.rect_filled(Rect::from_min_max(Pos2::new(rect.left(), r.top()), Pos2::new(r.left(), r.bottom())), 0.0, dim);
                painter.rect_filled(Rect::from_min_max(Pos2::new(r.right(), r.top()), Pos2::new(rect.right(), r.bottom())), 0.0, dim);
                painter.rect_stroke(r, 0.0, Stroke::new(2.0, ACCENT), egui::StrokeKind::Inside);

                // Drag to move, scroll to resize.
                let scale = f64::from(info.width) / f64::from(rect.width());
                let (mut cx, mut cy) = c.center();
                let mut side = f64::from(c.size);
                let mut changed = false;
                if response.dragged() {
                    let d = response.drag_delta();
                    cx += f64::from(d.x) * scale;
                    cy += f64::from(d.y) * scale;
                    changed = true;
                }
                if response.hovered() {
                    let scroll = ui.input(|i| i.smooth_scroll_delta.y);
                    if scroll != 0.0 {
                        side *= (1.0 + f64::from(scroll) * 0.002).clamp(0.5, 2.0);
                        changed = true;
                    }
                }
                if changed {
                    self.crop = Crop::around(cx, cy, side, info.width, info.height);
                }
            }

            let mut frame = match slot {
                Slot::Start => self.start.frame,
                Slot::End => self.end.frame,
            };
            let before = frame;
            ui.horizontal(|ui| {
                if ui.button("-1").on_hover_text("Previous frame").clicked() {
                    frame = frame.saturating_sub(1);
                }
                let slider = egui::Slider::new(&mut frame, 0..=last)
                    .show_value(true)
                    .custom_formatter(move |v, _| format!("{} (#{})", timecode::format(rate.time_of(v as u64)), v as u64))
                    .custom_parser(move |text| {
                        let text = text.trim();
                        match text.strip_prefix('#') {
                            Some(n) => n.trim().parse::<u64>().ok().map(|n| n as f64),
                            None => timecode::parse(text).map(|s| rate.frame_at(s) as f64),
                        }
                    });
                ui.spacing_mut().slider_width = (width - 150.0).max(80.0);
                ui.add(slider);
                if ui.button("+1").on_hover_text("Next frame").clicked() {
                    frame = (frame + 1).min(last);
                }
            });
            if frame != before {
                self.set_frame(slot, frame);
            }
        });
    }

    fn crop_ui(&mut self, ui: &mut egui::Ui, info: &VideoInfo) {
        ui.horizontal(|ui| {
            ui.checkbox(&mut self.crop_on, "Crop to a square");
            if self.crop_on {
                ui.weak("drag the square on a frame to move it, scroll over it to resize");
            }
        });
        if !self.crop_on {
            ui.weak("Without a crop the pump display shows the centered square.");
            return;
        }
        ui.horizontal(|ui| {
            // What the pump will show: the crop area of the Start frame.
            let (rect, _) = ui.allocate_exact_size(Vec2::splat(150.0), Sense::hover());
            match &self.start.texture {
                Some((_, tex)) => {
                    let c = self.crop;
                    let uv = Rect::from_min_max(
                        Pos2::new(c.x as f32 / info.width as f32, c.y as f32 / info.height as f32),
                        Pos2::new((c.x + c.size) as f32 / info.width as f32, (c.y + c.size) as f32 / info.height as f32),
                    );
                    ui.painter().image(tex.id(), rect, uv, Color32::WHITE);
                }
                None => {
                    ui.painter().rect_filled(rect, 0.0, Color32::from_gray(20));
                }
            }
            ui.vertical(|ui| {
                ui.label(RichText::new("What the pump shows").strong());
                let max = info.width.min(info.height) & !1;
                let mut size = self.crop.size;
                if ui.add(egui::Slider::new(&mut size, crop::MIN_SIZE.min(max)..=max).text("size (px)")).changed() {
                    let (cx, cy) = self.crop.center();
                    self.crop = Crop::around(cx, cy, f64::from(size), info.width, info.height);
                }
                ui.horizontal(|ui| {
                    if ui.button("Center").clicked() {
                        self.crop = Crop::around(f64::from(info.width) / 2.0, f64::from(info.height) / 2.0, f64::from(self.crop.size), info.width, info.height);
                    }
                    if ui.button("Largest").clicked() {
                        self.crop = Crop::centered_max(info.width, info.height);
                    }
                });
                ui.weak(format!("{} {TIMES} {} at ({}, {})", self.crop.size, self.crop.size, self.crop.x, self.crop.y));
            });
        });
    }

    fn search_ui(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, info: &VideoInfo, busy: bool) {
        ui.horizontal(|ui| {
            ui.add_enabled_ui(!busy, |ui| {
                if ui.button("Find best loop near these frames").clicked() {
                    self.find(ctx);
                }
                ui.add(egui::Slider::new(&mut self.radius, 0.5..=MAX_RADIUS).text("\u{b1} seconds").fixed_decimals(1));
            });
        });
        let Some(search) = &self.search else { return };
        let rate = info.rate;
        let mut pick = None;
        egui::ScrollArea::vertical().max_height(150.0).show(ui, |ui| {
            egui::Grid::new("candidates").striped(true).num_columns(4).show(ui, |ui| {
                for h in ["Start", "End (cut)", "Length", "Match"] {
                    ui.label(RichText::new(h).strong());
                }
                ui.end_row();
                for c in &search.candidates {
                    let current = c.start == self.start.frame && c.end == self.end.frame;
                    let start = format!("{} (#{})", timecode::format(rate.time_of(c.start)), c.start);
                    if ui.selectable_label(current, start).clicked() {
                        pick = Some(*c);
                    }
                    ui.label(format!("{} (#{})", timecode::format(rate.time_of(c.end)), c.end));
                    ui.label(timecode::format(rate.time_of(c.frames())));
                    ui.label(RichText::new(match_text(c.score)).color(color_for(c.score)));
                    ui.end_row();
                }
            });
        });
        if let Some(c) = pick {
            self.set_frame(Slot::Start, c.start);
            self.set_frame(Slot::End, c.end);
        }
    }

    fn output_ui(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, info: &VideoInfo, busy: bool) {
        ui.horizontal(|ui| {
            ui.label("Save as");
            ui.add(egui::TextEdit::singleline(&mut self.output).desired_width(420.0));
            if ui.button(format!("Browse{ELLIPSIS}")).clicked() {
                let default = PathBuf::from(&self.output);
                let mut dialog = rfd::FileDialog::new().add_filter("MP4 video", &["mp4"]);
                if let Some(dir) = default.parent() {
                    dialog = dialog.set_directory(dir);
                }
                if let Some(name) = default.file_name() {
                    dialog = dialog.set_file_name(name.to_string_lossy());
                }
                if let Some(path) = dialog.save_file() {
                    self.output = path.display().to_string();
                }
            }
        });
        ui.horizontal(|ui| {
            ui.add_enabled(info.has_audio, egui::Checkbox::new(&mut self.audio, "Include audio"));
            ui.add_enabled(
                self.crop_on,
                egui::Checkbox::new(&mut self.scale_to_display, format!("Scale to {DISPLAY_SIZE}{TIMES}{DISPLAY_SIZE} (pump display)")),
            );
            if Path::new(self.output.trim()).exists() {
                ui.colored_label(Color32::from_rgb(0xE8, 0xB3, 0x4B), "exists, will be overwritten");
            }
        });
        let valid = self.end.frame > self.start.frame;
        let can_save = !busy && valid && !self.output.trim().is_empty();
        ui.horizontal(|ui| {
            if ui.add_enabled(can_save, egui::Button::new("Save loop")).clicked() {
                self.save(ctx);
            }
            if !valid {
                ui.colored_label(RED, "End must come after Start");
            }
        });
        if let Some(path) = self.saved.clone() {
            ui.colored_label(GREEN, format!("Saved {}", path.display()));
            ui.horizontal(|ui| {
                if ui.add_enabled(!busy, egui::Button::new("Show on pump display")).clicked() {
                    self.show_on_display(ctx, path.clone());
                }
                if ui.button("Open folder").clicked() {
                    let _ = std::process::Command::new("explorer").arg(format!("/select,{}", path.display())).spawn();
                }
            });
        }
    }
}

impl eframe::App for LoopApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.finish_job(&ctx);
        self.poll_frames(&ctx);
        let dropped = ctx.input(|i| {
            i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).find(|p| !p.as_os_str().is_empty())
        });
        if let Some(path) = dropped
            && !self.busy()
        {
            self.open(&ctx, path);
        }
        let busy = self.busy();

        egui::CentralPanel::default().show(ui, |ui| {
            if let Err(e) = &self.tools {
                ui.colored_label(RED, e);
                return;
            }
            ui.horizontal(|ui| {
                if ui.add_enabled(!busy, egui::Button::new(format!("Open video{ELLIPSIS}"))).clicked()
                    && let Some(path) = rfd::FileDialog::new().add_filter("Videos", VIDEO_EXTS).pick_file()
                {
                    self.open(&ctx, path);
                }
                ui.weak("or drop a file on this window");
            });
            let Some(info) = self.info.clone() else {
                ui.add_space(8.0);
                ui.label("Open a video, pick roughly where the loop starts and ends, and fine-tune until the frames match.");
                if self.for_display {
                    ui.weak("Started from AIO Display: the crop is set up for the pump screen.");
                }
                return;
            };
            ui.separator();
            Self::info_ui(ui, &info);
            ui.add_space(4.0);

            let width = ((ui.available_width() - 16.0) / 2.0).max(200.0);
            ui.horizontal_top(|ui| {
                self.viewer_ui(ui, Slot::Start, "Start", "first frame of the loop", width, &info);
                ui.add_space(8.0);
                self.viewer_ui(ui, Slot::End, "End", "cut here: should look like Start", width, &info);
            });
            ui.horizontal(|ui| {
                match self.live_score() {
                    Some(score) => ui.label(RichText::new(match_text(score)).color(color_for(score)).strong()),
                    None => ui.weak(format!("comparing{ELLIPSIS}")),
                };
                if self.end.frame > self.start.frame {
                    let n = self.end.frame - self.start.frame;
                    ui.weak(format!("|  loop: {n} frames, {}", timecode::format(info.rate.time_of(n))));
                }
            });

            ui.separator();
            self.crop_ui(ui, &info);
            ui.separator();
            self.search_ui(ui, &ctx, &info, busy);
            ui.separator();
            self.output_ui(ui, &ctx, &info, busy);

            let job = self.job.lock().unwrap();
            if job.running {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(&job.stage);
                });
                if let Some(p) = job.progress {
                    ui.add(egui::ProgressBar::new(p).show_percentage());
                }
            }
            drop(job);
            if let Some((ok, text)) = &self.message {
                ui.colored_label(if *ok { GREEN } else { RED }, text);
            }
        });
    }
}

/// Opens the window, loading `video` first if given. `for_display` (set by
/// AIO Display) turns the crop on, preset to what the pump shows now.
pub fn run(video: Option<PathBuf>, for_display: bool) -> eframe::Result {
    let options = eframe::NativeOptions {
        renderer: eframe::Renderer::Glow,
        viewport: egui::ViewportBuilder::default()
            .with_title("AIO Loop Finder")
            .with_inner_size([1040.0, 940.0])
            .with_min_inner_size([760.0, 700.0])
            .with_drag_and_drop(true),
        ..Default::default()
    };
    eframe::run_native(
        "AIO Loop Finder",
        options,
        Box::new(move |cc| {
            let mut app = LoopApp::new(for_display);
            if let Some(path) = video {
                app.open(&cc.egui_ctx, path);
            }
            Ok(Box::new(app))
        }),
    )
}
