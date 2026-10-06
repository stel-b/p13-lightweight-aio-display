//! The loop finder window. Non-ASCII characters are written as escapes so no
//! tool's code page can mangle them.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use aio_loop::ffmpeg::{self, Tools, VideoInfo};
use aio_loop::matcher::quality;
use aio_loop::{MAX_RADIUS, Search, Stage, search, timecode};
use eframe::egui;
use egui::{Color32, RichText, TextureHandle, TextureOptions};

const ELLIPSIS: &str = "\u{2026}";
const RED: Color32 = Color32::from_rgb(0xE0, 0x6C, 0x5C);
const GREEN: Color32 = Color32::from_rgb(0x4C, 0xC3, 0x6E);
const VIDEO_EXTS: &[&str] = &["mp4", "m4v", "mkv", "webm", "mov", "avi", "wmv", "gif"];

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

struct LoopApp {
    tools: Result<Arc<Tools>, String>,
    job: Arc<Mutex<Job>>,
    info: Option<VideoInfo>,
    start: f64,
    end: f64,
    radius: f64,
    search: Option<Search>,
    selected: usize,
    textures: HashMap<u64, TextureHandle>,
    output: String,
    audio: bool,
    saved: Option<PathBuf>,
    message: Option<(bool, String)>,
}

fn color_for(score: f32) -> Color32 {
    match quality(score) {
        "excellent" | "good" => GREEN,
        "fair" => Color32::from_rgb(0xE8, 0xB3, 0x4B),
        _ => RED,
    }
}

impl LoopApp {
    fn new() -> Self {
        Self {
            tools: ffmpeg::find_tools().map(Arc::new).map_err(|e| format!("{e:#}")),
            job: Arc::new(Mutex::new(Job::default())),
            info: None,
            start: 0.0,
            end: 0.0,
            radius: 2.0,
            search: None,
            selected: 0,
            textures: HashMap::new(),
            output: String::new(),
            audio: false,
            saved: None,
            message: None,
        }
    }

    fn busy(&self) -> bool {
        self.job.lock().unwrap().running
    }

    /// Runs `work` on a thread; its result arrives in `Job::done`.
    fn spawn(&self, ctx: &egui::Context, stage: &str, work: impl FnOnce(&Mutex<Job>, &egui::Context) -> Done + Send + 'static) {
        {
            let mut job = self.job.lock().unwrap();
            *job = Job { running: true, stage: stage.to_owned(), progress: None, done: None };
        }
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

    fn find(&mut self, ctx: &egui::Context) {
        let (Ok(tools), Some(info)) = (self.tools.clone(), self.info.clone()) else { return };
        let (start, end, radius) = (self.start, self.end, self.radius);
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
        let (Ok(tools), Some(info), Some(search)) = (self.tools.clone(), self.info.clone(), self.search.as_ref()) else {
            return;
        };
        let candidate = search.candidates[self.selected];
        let output = PathBuf::from(self.output.trim());
        let audio = self.audio;
        self.saved = None;
        self.message = None;
        self.spawn(ctx, &format!("Saving loop{ELLIPSIS}"), move |job, ctx| {
            let result = ffmpeg::cut(&tools, &info, candidate.start, candidate.end, &output, audio, |p| {
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

    fn finish_job(&mut self) {
        let Some(done) = self.job.lock().unwrap().done.take() else { return };
        match done {
            Done::Info(Ok(info)) => {
                self.start = (info.duration * 0.1).min(1.0);
                self.end = info.duration;
                self.output = ffmpeg::loop_path(&info.path).display().to_string();
                self.audio = false;
                self.search = None;
                self.saved = None;
                self.textures.clear();
                self.info = Some(info);
            }
            Done::Search(Ok(search)) => {
                self.search = Some(search);
                self.selected = 0;
                self.textures.clear();
            }
            Done::Saved(Ok(path)) => self.saved = Some(path),
            Done::Shown(Ok(())) => self.message = Some((true, "Now playing on the pump display.".into())),
            Done::Info(Err(e)) | Done::Search(Err(e)) | Done::Saved(Err(e)) | Done::Shown(Err(e)) => {
                self.message = Some((false, e));
            }
        }
    }

    fn texture(&mut self, ctx: &egui::Context, n: u64) -> Option<TextureHandle> {
        if let Some(t) = self.textures.get(&n) {
            return Some(t.clone());
        }
        let search = self.search.as_ref()?;
        let rgb = search.frame(n)?;
        let image = egui::ColorImage::from_rgb([search.preview_width as usize, search.preview_height as usize], rgb);
        let tex = ctx.load_texture(format!("frame{n}"), image, TextureOptions::LINEAR);
        self.textures.insert(n, tex.clone());
        Some(tex)
    }

    fn frame_ui(&mut self, ui: &mut egui::Ui, title: &str, n: u64, caption: String) {
        let ctx = ui.ctx().clone();
        ui.vertical(|ui| {
            ui.label(RichText::new(title).strong());
            match (self.texture(&ctx, n), self.search.as_ref()) {
                (Some(tex), Some(s)) => {
                    let size = egui::vec2(160.0, 160.0 * s.preview_height as f32 / s.preview_width as f32);
                    ui.add(egui::Image::new(egui::load::SizedTexture::new(tex.id(), size)));
                }
                _ => {
                    ui.weak("(outside the decoded window)");
                }
            }
            ui.weak(caption);
        });
    }

    fn info_ui(ui: &mut egui::Ui, info: &VideoInfo) {
        let name = info.path.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        ui.label(RichText::new(name).strong());
        ui.weak(format!(
            "{}x{}  |  {:.3} fps  |  {}  |  {} frames  |  {}{}",
            info.width,
            info.height,
            info.rate.fps(),
            timecode::format(info.duration),
            info.frames,
            info.codec,
            if info.has_audio { "  |  audio" } else { "" }
        ));
    }

    fn time_slider<'a>(value: &'a mut f64, max: f64, label: &'a str) -> egui::Slider<'a> {
        egui::Slider::new(value, 0.0..=max)
            .text(label)
            .custom_formatter(|v, _| timecode::format(v))
            .custom_parser(timecode::parse)
    }

    fn results_ui(&mut self, ui: &mut egui::Ui, info: &VideoInfo) {
        let Some(search) = &self.search else { return };
        let rate = info.rate;
        let mut selected = self.selected;
        egui::Grid::new("candidates").striped(true).num_columns(5).show(ui, |ui| {
            for h in ["", "Start", "End (not included)", "Length", "Difference"] {
                ui.label(RichText::new(h).strong());
            }
            ui.end_row();
            for (i, c) in search.candidates.iter().enumerate() {
                ui.radio_value(&mut selected, i, format!("{}", i + 1));
                ui.label(format!("{}  (#{})", timecode::format(rate.time_of(c.start)), c.start));
                ui.label(format!("{}  (#{})", timecode::format(rate.time_of(c.end)), c.end));
                ui.label(format!("{}  ({} fr)", timecode::format(rate.time_of(c.frames())), c.frames()));
                ui.label(RichText::new(format!("{:.2}%  {}", c.score * 100.0, quality(c.score))).color(color_for(c.score)));
                ui.end_row();
            }
        });
        self.selected = selected;

        let c = search.candidates[self.selected];
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            self.frame_ui(ui, "First frame", c.start, format!("#{}", c.start));
            self.frame_ui(ui, "Last frame kept", c.end - 1, format!("#{}", c.end - 1));
            self.frame_ui(ui, "Next frame (cut)", c.end, format!("#{} \u{2248} first frame", c.end));
        });
    }

    fn output_ui(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, info: &VideoInfo, busy: bool) {
        ui.horizontal(|ui| {
            ui.label("Save as");
            ui.add(egui::TextEdit::singleline(&mut self.output).desired_width(300.0));
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
            let out = Path::new(self.output.trim());
            if out.exists() {
                ui.colored_label(Color32::from_rgb(0xE8, 0xB3, 0x4B), "exists, will be overwritten");
            }
        });
        let can_save = !busy && !self.output.trim().is_empty() && self.search.is_some();
        if ui.add_enabled(can_save, egui::Button::new("Save loop")).clicked() {
            self.save(ctx);
        }
        if let Some(path) = self.saved.clone() {
            ui.horizontal(|ui| {
                ui.colored_label(GREEN, format!("Saved {}", path.display()));
            });
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
        self.finish_job();
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
            egui::ScrollArea::vertical().show(ui, |ui| {
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
                    ui.label("Open a video, mark roughly where the loop starts and ends, and this finds the frames that join seamlessly.");
                    return;
                };
                ui.separator();
                Self::info_ui(ui, &info);

                ui.add_space(6.0);
                ui.add_enabled_ui(!busy, |ui| {
                    ui.add(Self::time_slider(&mut self.start, info.duration, "Start (approx.)"));
                    ui.add(Self::time_slider(&mut self.end, info.duration, "End (approx.)"));
                    ui.add(egui::Slider::new(&mut self.radius, 0.5..=MAX_RADIUS).text("Search \u{00b1} seconds").fixed_decimals(1));
                    if ui.button("Find loop").clicked() {
                        self.find(&ctx);
                    }
                });

                if self.search.is_some() {
                    ui.separator();
                    self.results_ui(ui, &info);
                    ui.separator();
                    self.output_ui(ui, &ctx, &info, busy);
                }

                ui.add_space(6.0);
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
        });
    }
}

/// Opens the window, loading `video` first if given.
pub fn run(video: Option<PathBuf>) -> eframe::Result {
    let options = eframe::NativeOptions {
        renderer: eframe::Renderer::Glow,
        viewport: egui::ViewportBuilder::default()
            .with_title("AIO Loop Finder")
            .with_inner_size([620.0, 760.0])
            .with_min_inner_size([560.0, 480.0])
            .with_drag_and_drop(true),
        ..Default::default()
    };
    eframe::run_native(
        "AIO Loop Finder",
        options,
        Box::new(move |cc| {
            let mut app = LoopApp::new();
            if let Some(path) = video {
                app.open(&cc.egui_ctx, path);
            }
            Ok(Box::new(app))
        }),
    )
}
