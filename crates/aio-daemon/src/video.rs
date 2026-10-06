//! Video import through the `ffmpeg` command-line tool (never linked).
//!
//! ffmpeg decodes, resamples to a constant frame rate (the source's, capped at
//! the panel's 60 Hz), crops the centered square and scales it to 480×480,
//! writing raw RGB to a pipe. Each
//! frame is then encoded with the same JPEG settings as images and GIFs, so
//! the device only ever sees one kind of frame.

use std::io::{self, Read};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use aio_proto::{HEIGHT, WIDTH};
use anyhow::{Context, Result, bail};
use tracing::{debug, info};

use crate::render;

/// Environment variable with the full path of ffmpeg.exe. The installer (or
/// `scripts/install.ps1 -VideoSupport`) sets it for the service, because a
/// per-user ffmpeg install is not on LocalSystem's PATH. ffprobe.exe is
/// expected next to it.
pub const FFMPEG_ENV: &str = "AIO_FFMPEG";
/// Highest frame rate we import: the panel refreshes at 60 Hz.
const MAX_FPS: u32 = 60;
/// Used when ffprobe can't tell (missing tool, odd container).
const FALLBACK_FPS: Rate = Rate { num: 30, den: 1 };
const FRAME_BYTES: usize = WIDTH as usize * HEIGHT as usize * 3;
/// Keeps ffmpeg/ffprobe from flashing a console window.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub struct Tools {
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
}

fn on_path(exe: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?).map(|dir| dir.join(exe)).find(|p| p.is_file())
}

/// Finds ffmpeg: `AIO_FFMPEG` first, then PATH.
pub fn find_tools() -> Result<Tools> {
    let ffmpeg = match std::env::var_os(FFMPEG_ENV) {
        Some(p) => {
            let p = PathBuf::from(p);
            anyhow::ensure!(
                p.is_file(),
                "ffmpeg is no longer at {}: reinstall it, or run the AIO Display installer again with \"Video support\" ticked",
                p.display()
            );
            p
        }
        None => on_path("ffmpeg.exe").context(
            "video support is not set up: install ffmpeg (winget install Gyan.FFmpeg), then run the AIO Display installer again with \"Video support\" ticked",
        )?,
    };
    let sibling = ffmpeg.with_file_name("ffprobe.exe");
    let ffprobe = if sibling.is_file() { sibling } else { on_path("ffprobe.exe").unwrap_or(sibling) };
    Ok(Tools { ffmpeg, ffprobe })
}

/// A frame rate as a fraction, e.g. 30000/1001.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rate {
    pub num: u32,
    pub den: u32,
}

impl Rate {
    pub fn parse(s: &str) -> Option<Rate> {
        let (num, den) = s.trim().split_once('/').unwrap_or((s.trim(), "1"));
        let rate = Rate { num: num.parse().ok()?, den: den.parse().ok()? };
        (rate.num > 0 && rate.den > 0).then_some(rate)
    }

    /// The source rate, capped at 60 fps.
    pub fn capped(self) -> Rate {
        if u64::from(self.num) > u64::from(MAX_FPS) * u64::from(self.den) {
            Rate { num: MAX_FPS, den: 1 }
        } else {
            self
        }
    }

    /// When frame `i` starts, in µs. Rounding per timestamp (not per frame)
    /// keeps e.g. 29.97 fps exact over any length.
    pub fn frame_start_us(self, i: u64) -> u64 {
        i * u64::from(self.den) * 1_000_000 / u64::from(self.num)
    }

    pub fn frame_duration_us(self, i: u64) -> u64 {
        self.frame_start_us(i + 1) - self.frame_start_us(i)
    }
}

/// Reads the average frame rate of the first video stream.
fn probe_rate(tools: &Tools, path: &Path) -> Option<Rate> {
    let out = Command::new(&tools.ffprobe)
        .args(["-v", "error", "-select_streams", "v:0", "-show_entries", "stream=avg_frame_rate,r_frame_rate"])
        .args(["-of", "default=noprint_wrappers=1"])
        .arg(path)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .ok()?;
    parse_probe(&String::from_utf8_lossy(&out.stdout))
}

/// Prefers `avg_frame_rate`; variable-rate files often report a bogus
/// `r_frame_rate` like 90000/1.
fn parse_probe(text: &str) -> Option<Rate> {
    let field = |name: &str| {
        text.lines().find_map(|l| l.strip_prefix(name)?.strip_prefix('=')).and_then(Rate::parse)
    };
    field("avg_frame_rate").or_else(|| field("r_frame_rate"))
}

/// fps → centered square crop (crop centers by default) → 480×480.
fn video_filter(rate: Rate) -> String {
    format!(
        "fps={}/{},crop=w='min(iw,ih)':h='min(iw,ih)',scale={WIDTH}:{HEIGHT}:flags=bicubic",
        rate.num, rate.den
    )
}

/// Kills ffmpeg if we bail out early, so it never lingers.
struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Fills `buf` completely, or returns `Ok(false)` at a clean end of stream.
fn read_frame(r: &mut impl Read, buf: &mut [u8]) -> io::Result<bool> {
    let mut filled = 0;
    while filled < buf.len() {
        match r.read(&mut buf[filled..])? {
            0 if filled == 0 => return Ok(false),
            0 => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "truncated frame from ffmpeg")),
            n => filled += n,
        }
    }
    Ok(true)
}

/// Decodes `path` and hands each panel-ready JPEG and its duration (µs) to `push`.
pub fn import(path: &Path, mut push: impl FnMut(&[u8], u64) -> Result<()>) -> Result<()> {
    let tools = find_tools()?;
    let rate = match probe_rate(&tools, path) {
        Some(r) => r.capped(),
        None => {
            debug!("could not read the frame rate; using {} fps", FALLBACK_FPS.num);
            FALLBACK_FPS
        }
    };
    info!(path = %path.display(), fps = format!("{}/{}", rate.num, rate.den), "importing video");

    let filter = video_filter(rate);
    let child = Command::new(&tools.ffmpeg)
        .args(["-nostdin", "-hide_banner", "-loglevel", "error", "-i"])
        .arg(path)
        .args(["-an", "-sn", "-dn", "-vf", &filter, "-pix_fmt", "rgb24", "-f", "rawvideo", "-"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .with_context(|| format!("starting {}", tools.ffmpeg.display()))?;
    let mut child = KillOnDrop(child);

    // Drain stderr on a thread so a chatty ffmpeg can't block on a full pipe.
    let mut stderr = child.0.stderr.take().expect("stderr is piped");
    let stderr_thread = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stderr.read_to_string(&mut text);
        text
    });

    let mut stdout = child.0.stdout.take().expect("stdout is piped");
    let mut rgb = vec![0u8; FRAME_BYTES];
    let mut frames = 0u64;
    while read_frame(&mut stdout, &mut rgb)? {
        push(&render::encode_rgb(&rgb)?, rate.frame_duration_us(frames))?;
        frames += 1;
    }
    drop(stdout);

    let status = child.0.wait()?;
    let errors = stderr_thread.join().unwrap_or_default();
    if !status.success() {
        let tail: Vec<&str> = errors.lines().rev().take(5).collect();
        bail!("ffmpeg failed ({status}): {}", tail.into_iter().rev().collect::<Vec<_>>().join(" / "));
    }
    if frames == 0 {
        bail!("no video frames in {}", path.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rates() {
        assert_eq!(Rate::parse("30000/1001"), Some(Rate { num: 30000, den: 1001 }));
        assert_eq!(Rate::parse("25"), Some(Rate { num: 25, den: 1 }));
        assert_eq!(Rate::parse("0/0"), None);
        assert_eq!(Rate::parse("abc"), None);
    }

    #[test]
    fn caps_at_60() {
        assert_eq!(Rate { num: 120, den: 1 }.capped(), Rate { num: 60, den: 1 });
        assert_eq!(Rate { num: 60000, den: 1001 }.capped(), Rate { num: 60000, den: 1001 });
        assert_eq!(Rate { num: 24, den: 1 }.capped(), Rate { num: 24, den: 1 });
    }

    #[test]
    fn durations_do_not_drift() {
        let ntsc = Rate { num: 30000, den: 1001 };
        let total: u64 = (0..30_000).map(|i| ntsc.frame_duration_us(i)).sum();
        assert_eq!(total, 1_001_000_000, "30000 frames of 29.97 fps are exactly 1001 s");

        let sixty = Rate { num: 60, den: 1 };
        let d: Vec<u64> = (0..3).map(|i| sixty.frame_duration_us(i)).collect();
        assert_eq!(d, [16_666, 16_667, 16_667]);
    }

    #[test]
    fn prefers_average_rate() {
        assert_eq!(
            parse_probe("r_frame_rate=90000/1\navg_frame_rate=30/1\n"),
            Some(Rate { num: 30, den: 1 })
        );
        assert_eq!(parse_probe("r_frame_rate=25/1\navg_frame_rate=0/0\n"), Some(Rate { num: 25, den: 1 }));
        assert_eq!(parse_probe(""), None);
    }

    #[test]
    fn reads_whole_frames_only() {
        let mut ok = &[1u8, 2, 3, 4, 5, 6][..];
        let mut buf = [0u8; 3];
        assert!(read_frame(&mut ok, &mut buf).unwrap());
        assert!(read_frame(&mut ok, &mut buf).unwrap());
        assert!(!read_frame(&mut ok, &mut buf).unwrap());

        let mut truncated = &[1u8, 2, 3, 4][..];
        assert!(read_frame(&mut truncated, &mut buf).unwrap());
        assert!(read_frame(&mut truncated, &mut buf).is_err());
    }
}
