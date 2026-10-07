//! Everything that touches the `ffmpeg` / `ffprobe` command-line tools.
//!
//! Frames are addressed by index; frame `n` starts at `n / fps`. To decode
//! from frame `n`, ffmpeg is asked to seek a quarter frame earlier, so the
//! first frame it returns is exactly `n` despite rounding. This assumes a
//! constant frame rate, which almost all video files have.

use std::io::{self, BufRead, BufReader, Read};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use anyhow::{Context, Result, bail};

use crate::Rate;
use crate::crop::Crop;

/// Keeps ffmpeg from flashing a console window from the GUI.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub struct Tools {
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
}

fn on_path(exe: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?).map(|dir| dir.join(exe)).find(|p| p.is_file())
}

/// `AIO_FFMPEG` (as for the daemon) first, then PATH.
pub fn find_tools() -> Result<Tools> {
    let ffmpeg = match std::env::var_os("AIO_FFMPEG") {
        Some(p) => PathBuf::from(p),
        None => on_path("ffmpeg.exe")
            .context("ffmpeg not found: install it (winget install Gyan.FFmpeg), then start the Loop Finder again")?,
    };
    let sibling = ffmpeg.with_file_name("ffprobe.exe");
    let ffprobe = if sibling.is_file() { sibling } else { on_path("ffprobe.exe").context("ffprobe not found")? };
    Ok(Tools { ffmpeg, ffprobe })
}

fn command(exe: &Path) -> Command {
    let mut cmd = Command::new(exe);
    cmd.creation_flags(CREATE_NO_WINDOW).stdin(Stdio::null());
    cmd
}

#[derive(Debug, Clone)]
pub struct VideoInfo {
    pub path: PathBuf,
    /// Displayed size (after any rotation metadata is applied).
    pub width: u32,
    pub height: u32,
    pub rate: Rate,
    pub duration: f64,
    pub frames: u64,
    pub codec: String,
    pub has_audio: bool,
}

impl VideoInfo {
    /// Height of a preview `width` pixels wide, kept even for ffmpeg.
    pub fn preview_height(&self, width: u32) -> u32 {
        let h = (f64::from(width) * f64::from(self.height) / f64::from(self.width.max(1))).round() as u32;
        (h + (h & 1)).max(2)
    }
}

pub fn probe(tools: &Tools, path: &Path) -> Result<VideoInfo> {
    let out = command(&tools.ffprobe)
        .args(["-v", "error", "-show_streams", "-show_format", "-of", "json"])
        .arg(path)
        .output()
        .context("running ffprobe")?;
    if !out.status.success() {
        bail!("ffprobe could not read {}: {}", path.display(), String::from_utf8_lossy(&out.stderr).trim());
    }
    parse_probe(path, &out.stdout)
}

fn parse_probe(path: &Path, json: &[u8]) -> Result<VideoInfo> {
    let v: serde_json::Value = serde_json::from_slice(json).context("parsing ffprobe output")?;
    let streams = v["streams"].as_array().cloned().unwrap_or_default();
    let video = streams
        .iter()
        .find(|s| s["codec_type"] == "video" && s["disposition"]["attached_pic"] != 1)
        .context("no video stream")?;
    let has_audio = streams.iter().any(|s| s["codec_type"] == "audio");
    let num = |x: &serde_json::Value| x.as_str().and_then(|s| s.parse::<f64>().ok()).or_else(|| x.as_f64());

    let rate = video["avg_frame_rate"]
        .as_str()
        .and_then(Rate::parse)
        .or_else(|| video["r_frame_rate"].as_str().and_then(Rate::parse))
        .context("unknown frame rate")?;
    let (mut width, mut height) = (
        video["width"].as_u64().context("unknown width")? as u32,
        video["height"].as_u64().context("unknown height")? as u32,
    );
    // Phone videos: ffmpeg applies the rotation when decoding, so swap.
    let rotation = video["side_data_list"]
        .as_array()
        .and_then(|l| l.iter().find_map(|d| d["rotation"].as_f64()))
        .or_else(|| num(&video["tags"]["rotate"]))
        .unwrap_or(0.0);
    if (rotation.abs() - 90.0).abs() < 1.0 || (rotation.abs() - 270.0).abs() < 1.0 {
        std::mem::swap(&mut width, &mut height);
    }
    let duration = num(&video["duration"]).or_else(|| num(&v["format"]["duration"])).context("unknown duration")?;
    let frames = num(&video["nb_frames"]).map(|f| f as u64).unwrap_or_else(|| (duration * rate.fps()).floor() as u64);
    Ok(VideoInfo {
        path: path.to_owned(),
        width,
        height,
        rate,
        duration,
        frames,
        codec: video["codec_name"].as_str().unwrap_or("?").to_owned(),
        has_audio,
    })
}

/// Seek target that makes ffmpeg's first output frame exactly frame `n`.
fn seek_to(rate: Rate, n: u64) -> String {
    let t = (n as f64 - 0.25).max(0.0) * f64::from(rate.den) / f64::from(rate.num);
    format!("{t:.6}")
}

struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Decodes `count` frames from frame `first`, scaled to `width` × preview
/// height, as packed RGB. May return fewer at the end of the video.
pub fn decode_frames(tools: &Tools, info: &VideoInfo, first: u64, count: u64, width: u32) -> Result<Vec<Vec<u8>>> {
    let height = info.preview_height(width);
    let frame_len = (width * height * 3) as usize;
    let mut child = KillOnDrop(
        command(&tools.ffmpeg)
            .args(["-v", "error", "-ss", &seek_to(info.rate, first), "-i"])
            .arg(&info.path)
            .args(["-frames:v", &count.to_string(), "-an", "-sn", "-dn"])
            .args(["-vf", &format!("scale={width}:{height}:flags=area,format=rgb24")])
            .args(["-f", "rawvideo", "-"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("starting ffmpeg")?,
    );
    let mut stdout = child.0.stdout.take().expect("piped");
    let mut frames = Vec::with_capacity(count as usize);
    loop {
        let mut buf = vec![0u8; frame_len];
        match read_full(&mut stdout, &mut buf)? {
            true => frames.push(buf),
            false => break,
        }
    }
    let mut err = String::new();
    if let Some(mut stderr) = child.0.stderr.take() {
        let _ = stderr.read_to_string(&mut err);
    }
    if !child.0.wait()?.success() {
        bail!("ffmpeg failed to decode: {}", err.trim());
    }
    if frames.is_empty() {
        bail!("no frames decoded at frame {first}");
    }
    Ok(frames)
}

fn read_full(r: &mut impl Read, buf: &mut [u8]) -> io::Result<bool> {
    let mut filled = 0;
    while filled < buf.len() {
        match r.read(&mut buf[filled..])? {
            0 if filled == 0 => return Ok(false),
            0 => return Ok(false), // truncated last frame: drop it
            n => filled += n,
        }
    }
    Ok(true)
}

/// Default output name: `clip.mov` → `clip_loop.mp4` next to it.
pub fn loop_path(input: &Path) -> PathBuf {
    let stem = input.file_stem().map_or_else(|| "video".into(), |s| s.to_string_lossy().into_owned());
    input.with_file_name(format!("{stem}_loop.mp4"))
}

/// What to write: which frames, optional square crop, optional scaling of
/// that square (e.g. to the pump's 480x480), and audio.
#[derive(Debug, Clone, Copy)]
pub struct CutOptions {
    pub start: u64,
    /// First frame not included.
    pub end: u64,
    pub crop: Option<Crop>,
    /// Side of the output square; only used together with `crop`.
    pub scale: Option<u32>,
    pub audio: bool,
}

impl CutOptions {
    pub fn frames(start: u64, end: u64) -> Self {
        Self { start, end, crop: None, scale: None, audio: false }
    }

    fn filter(&self) -> Option<String> {
        let crop = self.crop?;
        Some(match self.scale {
            Some(s) => format!("{},scale={s}:{s}:flags=lanczos", crop.filter()),
            None => crop.filter(),
        })
    }
}

/// Writes the frames to `output` (H.264, CRF 18), reporting progress as a
/// fraction 0..1.
pub fn cut(tools: &Tools, info: &VideoInfo, opts: &CutOptions, output: &Path, mut progress: impl FnMut(f32)) -> Result<()> {
    let (start, end) = (opts.start, opts.end);
    if end <= start {
        bail!("the loop must be at least one frame long");
    }
    if let Some(crop) = opts.crop
        && !crop.fits(info.width, info.height)
    {
        bail!("the crop {crop:?} does not fit in the {}x{} video", info.width, info.height);
    }
    let count = end - start;
    let seconds = info.rate.time_of(count);
    let mut cmd = command(&tools.ffmpeg);
    cmd.args(["-y", "-v", "error", "-nostats", "-progress", "pipe:1", "-ss", &seek_to(info.rate, start), "-i"])
        .arg(&info.path)
        .args(["-frames:v", &count.to_string(), "-t", &format!("{seconds:.6}"), "-sn", "-dn"]);
    if let Some(filter) = opts.filter() {
        cmd.args(["-vf", &filter]);
    }
    cmd.args(["-c:v", "libx264", "-crf", "18", "-preset", "medium", "-pix_fmt", "yuv420p"]);
    if opts.audio && info.has_audio {
        cmd.args(["-c:a", "aac", "-b:a", "192k"]);
    } else {
        cmd.arg("-an");
    }
    cmd.args(["-movflags", "+faststart"]).arg(output).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = KillOnDrop(cmd.spawn().context("starting ffmpeg")?);

    let mut stderr = child.0.stderr.take().expect("piped");
    let err_thread = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stderr.read_to_string(&mut text);
        text
    });
    for line in BufReader::new(child.0.stdout.take().expect("piped")).lines() {
        let line = line?;
        if let Some(us) = line.strip_prefix("out_time_us=").and_then(|v| v.parse::<f64>().ok()) {
            progress((us / 1e6 / seconds).clamp(0.0, 1.0) as f32);
        }
    }
    let status = child.0.wait()?;
    let err = err_thread.join().unwrap_or_default();
    if !status.success() {
        bail!("ffmpeg failed: {}", err.trim());
    }
    progress(1.0);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_probe_json() {
        let json = br#"{"streams":[
            {"codec_type":"video","codec_name":"h264","width":1920,"height":1080,
             "avg_frame_rate":"30000/1001","r_frame_rate":"30000/1001","duration":"10.010000","nb_frames":"300",
             "disposition":{"attached_pic":0}},
            {"codec_type":"audio","codec_name":"aac"}],
            "format":{"duration":"10.03"}}"#;
        let info = parse_probe(Path::new("x.mp4"), json).unwrap();
        assert_eq!((info.width, info.height, info.frames), (1920, 1080, 300));
        assert_eq!(info.rate, Rate { num: 30000, den: 1001 });
        assert!(info.has_audio);
        assert_eq!(info.preview_height(160), 90);
    }

    #[test]
    fn swaps_size_for_rotated_phone_video() {
        let json = br#"{"streams":[{"codec_type":"video","codec_name":"hevc","width":1920,"height":1080,
            "avg_frame_rate":"30/1","side_data_list":[{"rotation":-90}]}],"format":{"duration":"4.0"}}"#;
        let info = parse_probe(Path::new("x.mov"), json).unwrap();
        assert_eq!((info.width, info.height), (1080, 1920));
        assert_eq!(info.frames, 120);
    }

    #[test]
    fn seeks_a_quarter_frame_early() {
        let r = Rate { num: 30, den: 1 };
        assert_eq!(seek_to(r, 0), "0.000000");
        assert_eq!(seek_to(r, 30), "0.991667");
    }

    #[test]
    fn names_the_output() {
        assert_eq!(loop_path(Path::new(r"C:\v\clip.mov")), PathBuf::from(r"C:\v\clip_loop.mp4"));
    }
}
