//! The settings window.
//!
//! Non-ASCII characters are written as escapes so no tool's code page can
//! mangle them.

use std::path::PathBuf;

use aio_ipc::{DeviceState, Source, Status};
use eframe::egui;
use egui::{Color32, RichText, TextureHandle, TextureOptions};

use crate::backend::{Action, Backend};
use crate::tray::icon_rgba;

const PREVIEW_SIZE: f32 = 240.0;
const IMAGE_EXTS: &[&str] = &["png", "jpg", "jpeg", "bmp", "webp"];
const VIDEO_EXTS: &[&str] = &["mp4", "m4v", "mkv", "webm", "mov", "avi", "wmv"];
const DEGREE: &str = "\u{b0}";
const DOT: &str = "\u{25cf}";
const ELLIPSIS: &str = "\u{2026}";
const RED: Color32 = Color32::from_rgb(0xE0, 0x6C, 0x5C);

fn source_for(path: PathBuf) -> Source {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or_default().to_ascii_lowercase();
    if ext == "gif" {
        Source::Gif { path }
    } else if VIDEO_EXTS.contains(&ext.as_str()) {
        Source::Video { path }
    } else {
        Source::Image { path }
    }
}

fn describe(source: &Source) -> String {
    let name = |p: &PathBuf| p.file_name().map_or_else(|| p.display().to_string(), |n| n.to_string_lossy().into());
    match source {
        Source::Color { rgb: [r, g, b] } => format!("Color #{r:02x}{g:02x}{b:02x}"),
        Source::Image { path } => format!("Image: {}", name(path)),
        Source::Gif { path } => format!("GIF: {}", name(path)),
        Source::Video { path } => format!("Video: {}", name(path)),
    }
}

struct AioApp {
    backend: Backend,
    preview: Option<TextureHandle>,
    preview_generation: u64,
    color: [u8; 3],
    /// Slider value while dragging; sent when the drag ends.
    brightness: Option<u8>,
}

impl AioApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        Self {
            backend: Backend::start(cc.egui_ctx.clone()),
            preview: None,
            preview_generation: 0,
            color: [0x2E, 0xC4, 0xB6],
            brightness: None,
        }
    }

    fn pick_file(&self) {
        let all: Vec<&str> = IMAGE_EXTS.iter().chain(VIDEO_EXTS).chain(&["gif"]).copied().collect();
        let picked = rfd::FileDialog::new()
            .set_title("Choose an image, GIF or video")
            .add_filter("Images, GIFs and videos", &all)
            .add_filter("Images", IMAGE_EXTS)
            .add_filter("GIF", &["gif"])
            .add_filter("Videos", VIDEO_EXTS)
            .pick_file();
        if let Some(path) = picked {
            self.backend.send(Action::SetSource(source_for(path)));
        }
    }

    fn status_ui(ui: &mut egui::Ui, status: &Option<Result<Status, String>>) {
        let (dot, text) = match status {
            None => (Color32::GRAY, format!("Connecting to the daemon{ELLIPSIS}")),
            Some(Err(e)) => (RED, format!("Daemon not reachable: {e}")),
            Some(Ok(s)) => match s.device {
                DeviceState::Connected => (Color32::from_rgb(0x4C, 0xC3, 0x6E), "Display connected".to_owned()),
                DeviceState::Busy => (Color32::from_rgb(0xE8, 0xB3, 0x4B), "Display in use by another program".to_owned()),
                DeviceState::Disconnected => (RED, "Display disconnected".to_owned()),
            },
        };
        ui.horizontal(|ui| {
            ui.label(RichText::new(DOT).color(dot));
            ui.label(text);
        });
        if let Some(Ok(s)) = status {
            let mut parts = vec![format!("{:.0} fps", s.fps), format!("{} frames sent", s.frames_sent)];
            if let Some(fw) = &s.firmware {
                parts.push(format!("firmware {fw}"));
            }
            ui.weak(parts.join("  |  "));
            if let Some(err) = &s.last_error {
                ui.colored_label(RED, format!("Last error: {err}"));
            }
        }
    }

    fn preview_ui(&mut self, ui: &mut egui::Ui) {
        {
            let mut state = self.backend.state();
            if state.preview_generation != self.preview_generation {
                self.preview_generation = state.preview_generation;
                if let Some(image) = state.preview.take() {
                    match &mut self.preview {
                        Some(tex) => tex.set(image, TextureOptions::LINEAR),
                        None => self.preview = Some(ui.ctx().load_texture("preview", image, TextureOptions::LINEAR)),
                    }
                }
            }
        }
        ui.vertical_centered(|ui| match &self.preview {
            Some(tex) => {
                let image = egui::Image::new(egui::load::SizedTexture::new(tex.id(), [PREVIEW_SIZE, PREVIEW_SIZE]));
                ui.add(image.corner_radius(PREVIEW_SIZE / 2.0));
            }
            None => {
                let (rect, _) = ui.allocate_exact_size(egui::vec2(PREVIEW_SIZE, PREVIEW_SIZE), egui::Sense::hover());
                ui.painter().circle_filled(rect.center(), PREVIEW_SIZE / 2.0, Color32::from_gray(20));
                ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, "No preview", egui::FontId::proportional(14.0), Color32::GRAY);
            }
        });
    }

    fn controls_ui(&mut self, ui: &mut egui::Ui, status: Option<&Status>, busy: bool) {
        ui.add_enabled_ui(status.is_some() && !busy, |ui| {
            ui.heading("Source");
            ui.label(status.and_then(|s| s.source.as_ref()).map_or("Nothing set".into(), describe));
            ui.horizontal(|ui| {
                ui.color_edit_button_srgb(&mut self.color);
                if ui.button("Show color").clicked() {
                    self.backend.send(Action::SetSource(Source::Color { rgb: self.color }));
                }
                ui.separator();
                if ui.button(format!("Choose file{ELLIPSIS}")).clicked() {
                    self.pick_file();
                }
            });
            // The loop finder is a separate app next to this one.
            let loop_finder = std::env::current_exe().ok().map(|exe| exe.with_file_name("aio-loop.exe")).filter(|p| p.is_file());
            if let Some(exe) = loop_finder
                && ui.button(format!("Make a seamless video loop{ELLIPSIS}")).clicked()
            {
                // Crop preset for the pump, and the current video if one is playing.
                let mut cmd = std::process::Command::new(exe);
                cmd.arg("--for-display");
                if let Some(Source::Video { path }) = status.and_then(|s| s.source.as_ref()) {
                    cmd.arg(path);
                }
                let _ = cmd.spawn();
            }

            ui.add_space(8.0);
            ui.heading("Display");
            let paused = status.is_some_and(|s| s.paused);
            if ui.button(if paused { "Resume" } else { "Pause" }).clicked() {
                self.backend.send(if paused { Action::Resume } else { Action::Pause });
            }

            let current = status.map_or(100, |s| s.brightness);
            let mut value = self.brightness.unwrap_or(current);
            let slider = ui.add(egui::Slider::new(&mut value, 0..=100).text("Brightness"));
            if slider.changed() {
                self.brightness = Some(value);
            }
            if slider.drag_stopped() || (slider.changed() && !slider.dragged()) {
                self.backend.send(Action::Brightness(value));
            }
            if !slider.dragged() && self.brightness == Some(current) {
                self.brightness = None; // the daemon caught up
            }

            let rotation = status.and_then(|s| s.rotation);
            let label = |d: Option<u16>| d.map_or("Device default".to_owned(), |d| format!("{d}{DEGREE}"));
            egui::ComboBox::from_label("Rotation").selected_text(label(rotation)).show_ui(ui, |ui| {
                for degrees in [0u16, 90, 180, 270] {
                    if ui.selectable_label(rotation == Some(degrees), label(Some(degrees))).clicked() {
                        self.backend.send(Action::Rotation(degrees));
                    }
                }
            });
        });
    }
}

impl eframe::App for AioApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let (status, busy, last_result) = {
            let s = self.backend.state();
            (s.status.clone(), s.busy, s.last_result.clone())
        };
        egui::CentralPanel::default().show(ui, |ui| {
            Self::status_ui(ui, &status);
            ui.add_space(10.0);
            self.preview_ui(ui);
            ui.add_space(10.0);
            self.controls_ui(ui, status.as_ref().and_then(|s| s.as_ref().ok()), busy.is_some());
            ui.add_space(8.0);
            if let Some(text) = busy {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(text);
                });
            } else if let Some(Err(e)) = &last_result {
                ui.colored_label(RED, e);
            }
        });
    }
}

pub fn run() -> eframe::Result {
    // OpenGL: on an AMD GPU it used less memory than D3D12 via wgpu
    // (~240 MB vs ~280 MB private, almost all of it the graphics driver).
    let mut options = eframe::NativeOptions { renderer: eframe::Renderer::Glow, ..Default::default() };
    options.viewport = egui::ViewportBuilder::default()
        .with_title(crate::WINDOW_TITLE)
        .with_inner_size([380.0, 600.0])
        .with_min_inner_size([340.0, 520.0])
        .with_icon(egui::IconData { rgba: icon_rgba(64), width: 64, height: 64 });
    eframe::run_native(crate::WINDOW_TITLE, options, Box::new(|cc| Ok(Box::new(AioApp::new(cc)))))
}
