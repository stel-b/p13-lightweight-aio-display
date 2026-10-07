//! Messages between the daemon and its clients (CLI, UI).
//!
//! Transport: the named pipe [`PIPE_NAME`], one JSON object per line. The
//! client sends a [`Request`] and reads exactly one [`Response`] line back,
//! and may send more requests on the same connection.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[cfg(windows)]
mod client;
#[cfg(windows)]
pub use client::Client;

pub const PIPE_NAME: &str = r"\\.\pipe\aio-ui";
/// Longest request line the daemon accepts.
pub const MAX_REQUEST_LEN: usize = 64 * 1024;

/// What the display shows. Also the `[source]` table of the daemon config.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Source {
    Color { rgb: [u8; 3] },
    /// A still image; for an animated file only the first frame is used.
    Image { path: PathBuf },
    Gif { path: PathBuf },
    Video { path: PathBuf },
}

/// File extensions treated as video (everything else that isn't a GIF is an image).
pub const VIDEO_EXTENSIONS: &[&str] = &["mp4", "m4v", "mkv", "webm", "mov", "avi", "wmv"];
/// File extensions treated as still images.
pub const IMAGE_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "bmp", "webp"];

impl Source {
    /// The source for a media file, chosen by its extension.
    pub fn for_file(path: PathBuf) -> Source {
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or_default().to_ascii_lowercase();
        if ext == "gif" {
            Source::Gif { path }
        } else if VIDEO_EXTENSIONS.contains(&ext.as_str()) {
            Source::Video { path }
        } else {
            Source::Image { path }
        }
    }

    /// The file behind this source, if any.
    pub fn path(&self) -> Option<&std::path::Path> {
        match self {
            Source::Color { .. } => None,
            Source::Image { path } | Source::Gif { path } | Source::Video { path } => Some(path),
        }
    }

    /// Short human description, e.g. `GIF: cat.gif` or `Color #2ec4b6`.
    pub fn label(&self) -> String {
        let name = |p: &PathBuf| p.file_name().map_or_else(|| p.display().to_string(), |n| n.to_string_lossy().into_owned());
        match self {
            Source::Color { rgb: [r, g, b] } => format!("Color #{r:02x}{g:02x}{b:02x}"),
            Source::Image { path } => format!("Image: {}", name(path)),
            Source::Gif { path } => format!("GIF: {}", name(path)),
            Source::Video { path } => format!("Video: {}", name(path)),
        }
    }
}

/// What is shown when the daemon starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlayMode {
    /// The selected source (the last one shown).
    #[default]
    Selected,
    /// A random library item at every start (normally: every boot).
    Random,
}

/// One entry of the media library.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LibraryItem {
    pub id: u64,
    pub source: Source,
    /// False when neither its cached frames nor its file exist any more.
    pub available: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    /// Imports the source (may take a while for large GIFs), then shows it.
    SetSource { source: Source },
    /// Stops sending frames; the display keeps showing the current one.
    Pause,
    Resume,
    GetStatus,
    /// The frame currently on the display.
    GetPreview,
    /// Backlight level, 0..=100. Saved and applied at every connect.
    SetBrightness { value: u8 },
    /// Screen rotation in degrees: 0, 90, 180 or 270.
    SetRotation { degrees: u16 },
    /// Imports a file into the library (without showing it).
    LibraryAdd { source: Source },
    /// Removes an item from the library (the file itself is not touched).
    LibraryRemove { id: u64 },
    /// Shows a library item.
    LibraryShow { id: u64 },
    SetPlayMode { mode: PlayMode },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    Ok,
    Status(Status),
    Preview {
        #[serde(with = "base64_bytes")]
        jpeg: Vec<u8>,
    },
    Error { message: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceState {
    /// Not found, or reconnecting after an error.
    Disconnected,
    /// Another program holds the device (e.g. Steam).
    Busy,
    /// Handshake done; frames are being accepted.
    Connected,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Status {
    pub device: DeviceState,
    pub source: Option<Source>,
    pub paused: bool,
    /// Frames per second actually sent over the last second.
    pub fps: f32,
    pub frames_sent: u64,
    pub last_error: Option<String>,
    #[serde(default = "full_brightness")]
    pub brightness: u8,
    /// Configured rotation; `None` leaves the device's own setting.
    #[serde(default)]
    pub rotation: Option<u16>,
    /// Device firmware version, when HID control works.
    #[serde(default)]
    pub firmware: Option<String>,
    #[serde(default)]
    pub library: Vec<LibraryItem>,
    #[serde(default)]
    pub play_mode: PlayMode,
    /// The library item on the display, if the current source is one.
    #[serde(default)]
    pub current_item: Option<u64>,
}

fn full_brightness() -> u8 {
    100
}

/// Serializes a message as one line, newline included.
pub fn encode_line<T: Serialize>(msg: &T) -> Vec<u8> {
    let mut line = serde_json::to_vec(msg).expect("IPC messages always serialize");
    line.push(b'\n');
    line
}

pub fn decode_line<'a, T: Deserialize<'a>>(line: &'a str) -> serde_json::Result<T> {
    serde_json::from_str(line.trim_end())
}

/// Parses `#rrggbb`, `rrggbb` or a CSS color name.
pub fn parse_color(s: &str) -> Option<[u8; 3]> {
    let hex = s.strip_prefix('#').unwrap_or(s);
    if hex.len() == 6 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
        let v = u32::from_str_radix(hex, 16).ok()?;
        return Some([(v >> 16) as u8, (v >> 8) as u8, v as u8]);
    }
    let rgb = match s.to_ascii_lowercase().as_str() {
        "black" => [0, 0, 0],
        "white" => [255, 255, 255],
        "red" => [255, 0, 0],
        "lime" => [0, 255, 0],
        "green" => [0, 128, 0],
        "blue" => [0, 0, 255],
        "yellow" => [255, 255, 0],
        "cyan" | "aqua" => [0, 255, 255],
        "magenta" | "fuchsia" => [255, 0, 255],
        "orange" => [255, 165, 0],
        "purple" => [128, 0, 128],
        "pink" => [255, 192, 203],
        "teal" => [0, 128, 128],
        "navy" => [0, 0, 128],
        "gray" | "grey" => [128, 128, 128],
        _ => return None,
    };
    Some(rgb)
}

mod base64_bytes {
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&STANDARD.encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let text = <&str>::deserialize(d)?;
        STANDARD.decode(text).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_wire_format() {
        let req = Request::SetSource { source: Source::Gif { path: r"C:\a.gif".into() } };
        let line = encode_line(&req);
        assert_eq!(
            std::str::from_utf8(&line).unwrap(),
            "{\"type\":\"set_source\",\"source\":{\"type\":\"gif\",\"path\":\"C:\\\\a.gif\"}}\n"
        );
        assert_eq!(decode_line::<Request>(std::str::from_utf8(&line).unwrap()).unwrap(), req);
        assert_eq!(decode_line::<Request>("{\"type\":\"pause\"}").unwrap(), Request::Pause);
        assert_eq!(
            decode_line::<Request>("{\"type\":\"set_brightness\",\"value\":30}").unwrap(),
            Request::SetBrightness { value: 30 }
        );
    }

    #[test]
    fn responses_roundtrip() {
        let responses = [
            Response::Ok,
            Response::Error { message: "nope".into() },
            Response::Preview { jpeg: vec![0xFF, 0xD8, 0, 1, 2] },
            Response::Status(Status {
                device: DeviceState::Busy,
                source: Some(Source::Color { rgb: [1, 2, 3] }),
                paused: true,
                fps: 9.5,
                frames_sent: 42,
                last_error: Some("x".into()),
                brightness: 40,
                rotation: Some(180),
                firmware: Some("P13_20251204v01".into()),
                library: vec![LibraryItem { id: 3, source: Source::Gif { path: "C:\\a.gif".into() }, available: true }],
                play_mode: PlayMode::Random,
                current_item: Some(3),
            }),
        ];
        for r in responses {
            let line = encode_line(&r);
            assert_eq!(decode_line::<Response>(std::str::from_utf8(&line).unwrap()).unwrap(), r);
        }
    }

    #[test]
    fn preview_is_base64() {
        let line = encode_line(&Response::Preview { jpeg: vec![0xFF, 0xD8] });
        assert_eq!(std::str::from_utf8(&line).unwrap(), "{\"type\":\"preview\",\"jpeg\":\"/9g=\"}\n");
    }

    #[test]
    fn library_requests_roundtrip() {
        for req in [
            Request::LibraryAdd { source: Source::Video { path: r"C:\v.mp4".into() } },
            Request::LibraryRemove { id: 7 },
            Request::LibraryShow { id: 7 },
            Request::SetPlayMode { mode: PlayMode::Random },
        ] {
            let line = encode_line(&req);
            assert_eq!(decode_line::<Request>(std::str::from_utf8(&line).unwrap()).unwrap(), req);
        }
        assert_eq!(decode_line::<Request>("{\"type\":\"set_play_mode\",\"mode\":\"random\"}").unwrap(),
            Request::SetPlayMode { mode: PlayMode::Random });
    }

    #[test]
    fn older_status_without_library_still_parses() {
        let old = r#"{"device":"connected","source":null,"paused":false,"fps":0.0,"frames_sent":0,"last_error":null}"#;
        let s: Status = serde_json::from_str(old).unwrap();
        assert!(s.library.is_empty());
        assert_eq!(s.play_mode, PlayMode::Selected);
    }

    #[test]
    fn source_from_file_extension() {
        assert!(matches!(Source::for_file("a.GIF".into()), Source::Gif { .. }));
        assert!(matches!(Source::for_file("a.mkv".into()), Source::Video { .. }));
        assert!(matches!(Source::for_file("a.png".into()), Source::Image { .. }));
        assert_eq!(Source::for_file(r"C:\x\cat.gif".into()).label(), "GIF: cat.gif");
        assert_eq!(Source::Color { rgb: [0, 128, 255] }.label(), "Color #0080ff");
    }

    #[test]
    fn rejects_unknown_request() {
        assert!(decode_line::<Request>("{\"type\":\"firmware_update\"}").is_err());
    }

    #[test]
    fn parses_colors() {
        assert_eq!(parse_color("#ff8800"), Some([255, 136, 0]));
        assert_eq!(parse_color("00FF7f"), Some([0, 255, 127]));
        assert_eq!(parse_color("Teal"), Some([0, 128, 128]));
        assert_eq!(parse_color("green"), Some([0, 128, 0]));
        assert_eq!(parse_color("#12345"), None);
        assert_eq!(parse_color("nope"), None);
    }
}
