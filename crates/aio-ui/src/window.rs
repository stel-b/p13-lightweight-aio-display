//! The settings window.
//!
//! Non-ASCII characters are written as escapes so no tool's code page can
//! mangle them.

use aio_ipc::{DeviceState, IMAGE_EXTENSIONS, PlayMode, Source, Status, VIDEO_EXTENSIONS};
use eframe::egui;
use egui::{Color32, RichText, TextureHandle, TextureOptions};

use crate::backend::{Action, Backend};
use crate::tray::icon_rgba;

const PREVIEW_SIZE: f32 = 240.0;
const DEGREE: &str = "\u{b0}";
const DOT: &str = "\u{25cf}";
const ELLIPSIS: &str = "\u{2026}";
const RED: Color32 = Color32::from_rgb(0xE0, 0x6C, 0x5C);

/// File dialog for media files.
fn media_dialog(title: &str) -> rfd::FileDialog {
    let all: Vec<&str> = IMAGE_EXTENSIONS.iter().chain(VIDEO_EXTENSIONS).chain(&["gif"]).copied().collect();
    rfd::FileDialog::new()
        .set_title(title)
        .add_filter("Images, GIFs and videos", &all)
        .add_filter("Images", IMAGE_EXTENSIONS)
        .add_filter("GIF", &["gif"])
        .add_filter("Videos", VIDEO_EXTENSIONS)
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
        if let Some(path) = media_dialog("Choose an image, GIF or video").pick_file() {
            self.backend.send(Action::SetSource(Source::for_file(path)));
        }
    }

    fn add_to_library(&self) {
        // Each file is imported in turn by the command thread.
        for path in media_dialog("Add to the library").pick_files().unwrap_or_default() {
            self.backend.send(Action::LibraryAdd(Source::for_file(path)));
        }
    }

    fn library_ui(&mut self, ui: &mut egui::Ui, status: &Status) {
        ui.heading("Library");
        ui.horizontal(|ui| {
            ui.label("At startup show:");
            for (mode, text, tip) in [
                (PlayMode::Selected, "Last shown", "Keep showing what was shown last."),
                (PlayMode::Random, "Random item", "Pick a random library item every time the computer starts."),
            ] {
                if ui.radio(status.play_mode == mode, text).on_hover_text(tip).clicked() && status.play_mode != mode {
                    self.backend.send(Action::PlayMode(mode));
                }
            }
        });
        if status.library.is_empty() {
            ui.weak("Empty. Add GIFs, videos or images to switch between them.");
        }
        egui::ScrollArea::vertical().max_height(150.0).auto_shrink([false, true]).show(ui, |ui| {
            for item in &status.library {
                ui.horizontal(|ui| {
                    let current = status.current_item == Some(item.id);
                    let mut text = RichText::new(item.source.label());
                    if current {
                        text = text.strong();
                    }
                    if !item.available {
                        text = text.color(RED);
                    }
                    let path = item.source.path().map_or_else(String::new, |p| p.display().to_string());
                    let tip = if item.available { path } else { format!("{path}\nFile and cached frames are missing.") };
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("Remove").on_hover_text("Remove from the library (the file is kept)").clicked() {
                            self.backend.send(Action::LibraryRemove(item.id));
                        }
                        if ui.add_enabled(!current && item.available, egui::Button::new("Show").small()).clicked() {
                            self.backend.send(Action::LibraryShow(item.id));
                        }
                        ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                            ui.add(egui::Label::new(text).truncate()).on_hover_text(tip);
                        });
                    });
                });
            }
        });
        if ui.button(format!("Add to library{ELLIPSIS}")).clicked() {
            self.add_to_library();
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
            ui.label(status.and_then(|s| s.source.as_ref()).map_or("Nothing set".into(), Source::label));
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

            if let Some(status) = status {
                ui.add_space(8.0);
                self.library_ui(ui, status);
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
            egui::ScrollArea::vertical().show(ui, |ui| self.page_ui(ui, &status, busy, &last_result));
        });
    }
}

impl AioApp {
    fn page_ui(&mut self, ui: &mut egui::Ui, status: &Option<Result<Status, String>>, busy: Option<&str>, last_result: &Option<Result<(), String>>) {
        {
            Self::status_ui(ui, status);
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
            } else if let Some(Err(e)) = last_result {
                ui.colored_label(RED, e);
            }
        }
    }
}

pub fn run() -> eframe::Result {
    // OpenGL: on an AMD GPU it used less memory than D3D12 via wgpu
    // (~240 MB vs ~280 MB private, almost all of it the graphics driver).
    let mut options = eframe::NativeOptions { renderer: eframe::Renderer::Glow, ..Default::default() };
    options.viewport = egui::ViewportBuilder::default()
        .with_title(crate::WINDOW_TITLE)
        .with_inner_size([400.0, 800.0])
        .with_min_inner_size([340.0, 520.0])
        .with_icon(egui::IconData { rgba: icon_rgba(64), width: 64, height: 64 });
    eframe::run_native(crate::WINDOW_TITLE, options, Box::new(|cc| Ok(Box::new(AioApp::new(cc)))))
}
