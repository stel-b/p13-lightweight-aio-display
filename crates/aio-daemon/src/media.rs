//! Media import and the on-disk frame cache.
//!
//! All decoding and encoding happens here, once per source. Each import goes
//! to `cache/<key>/`:
//!
//! - `frames.bin`: the ready-to-send JPEGs, back to back;
//! - `index.json`: offset, length and display time of each frame.
//!
//! The key is a hash of the source's content, so re-setting the same file
//! reuses the cache and editing it creates a new one.

use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use image::AnimationDecoder;
use image::codecs::gif::GifDecoder;
use serde::{Deserialize, Serialize};
use tracing::info;

use crate::{render, video};

/// Bump when the cache layout or the encoding settings change.
pub const CACHE_VERSION: u32 = 3;
const INDEX_FILE: &str = "index.json";
const FRAMES_FILE: &str = "frames.bin";
/// Browsers show GIF frames with a delay of 10 ms or less for 100 ms; so do we.
/// (GIF delays are in 10 ms units, so the fastest real GIF is 20 ms / 50 fps.)
const DEFAULT_DELAY_MS: u32 = 100;
/// The panel refreshes at 60 Hz; frames shorter than one refresh are never seen.
const MIN_DELAY_MS: u32 = 16;

pub use aio_ipc::Source;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameEntry {
    pub offset: u64,
    pub len: u32,
    /// How long the frame stays on screen, in µs (fine enough for exact
    /// 29.97/59.94 fps video). Ignored for single-frame media.
    pub duration_us: u64,
}

#[derive(Serialize, Deserialize)]
struct Index {
    version: u32,
    frames: Vec<FrameEntry>,
}

/// An imported source, ready for playback.
pub struct CachedMedia {
    key: String,
    frames: Vec<FrameEntry>,
    file: File,
}

impl CachedMedia {
    /// Opens a cache entry, checking that the index matches the frame data.
    pub fn open(dir: &Path) -> io::Result<Self> {
        let invalid = |msg: &str| io::Error::new(io::ErrorKind::InvalidData, msg.to_owned());
        let index: Index = serde_json::from_slice(&fs::read(dir.join(INDEX_FILE))?)?;
        if index.version != CACHE_VERSION {
            return Err(invalid("cache version mismatch"));
        }
        if index.frames.is_empty() {
            return Err(invalid("no frames"));
        }
        let mut expected_offset = 0;
        for f in &index.frames {
            if f.offset != expected_offset || f.len == 0 {
                return Err(invalid("frame table is not contiguous"));
            }
            expected_offset += u64::from(f.len);
        }
        let file = File::open(dir.join(FRAMES_FILE))?;
        if file.metadata()?.len() != expected_offset {
            return Err(invalid("frame data size does not match index"));
        }
        let key = dir.file_name().unwrap_or_default().to_string_lossy().into_owned();
        Ok(Self { key, frames: index.frames, file })
    }

    pub fn key(&self) -> &str {
        &self.key
    }

    pub fn frames(&self) -> &[FrameEntry] {
        &self.frames
    }

    /// A single frame: send it once, then idle.
    pub fn is_static(&self) -> bool {
        self.frames.len() == 1
    }

    pub fn loop_duration(&self) -> Duration {
        Duration::from_micros(self.frames.iter().map(|f| f.duration_us).sum())
    }

    /// Reads frame `index` into `buf`, reusing its allocation.
    pub fn read_frame(&mut self, index: usize, buf: &mut Vec<u8>) -> io::Result<()> {
        let entry = self.frames[index];
        buf.resize(entry.len as usize, 0);
        self.file.seek(SeekFrom::Start(entry.offset))?;
        self.file.read_exact(buf)
    }
}

/// 64-bit FNV-1a: stable across Rust versions, unlike `DefaultHasher`.
struct Fnv(u64);

impl Fnv {
    fn new() -> Self {
        Fnv(0xcbf2_9ce4_8422_2325)
    }

    fn update(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0 ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3);
        }
    }

    fn update_reader(&mut self, mut r: impl Read) -> io::Result<()> {
        let mut buf = [0u8; 64 * 1024];
        loop {
            match r.read(&mut buf)? {
                0 => return Ok(()),
                n => self.update(&buf[..n]),
            }
        }
    }
}

/// Cache directory name for a source.
pub fn cache_key(source: &Source) -> io::Result<String> {
    let mut h = Fnv::new();
    h.update(&CACHE_VERSION.to_le_bytes());
    h.update(&[render::JPEG_QUALITY]);
    match source {
        Source::Color { rgb } => {
            h.update(b"color");
            h.update(rgb);
        }
        Source::Image { path } | Source::Gif { path } | Source::Video { path } => {
            h.update(match source {
                Source::Image { .. } => b"image",
                Source::Gif { .. } => b"gif__",
                _ => b"video",
            });
            h.update_reader(File::open(path)?)?;
        }
    }
    Ok(format!("{:016x}", h.0))
}

struct FrameWriter {
    out: BufWriter<File>,
    frames: Vec<FrameEntry>,
    offset: u64,
}

impl FrameWriter {
    fn create(path: &Path) -> io::Result<Self> {
        Ok(Self { out: BufWriter::new(File::create(path)?), frames: Vec::new(), offset: 0 })
    }

    fn push(&mut self, jpeg: &[u8], duration_us: u64) -> io::Result<()> {
        self.out.write_all(jpeg)?;
        self.frames.push(FrameEntry { offset: self.offset, len: jpeg.len() as u32, duration_us });
        self.offset += jpeg.len() as u64;
        Ok(())
    }

    fn finish(mut self) -> io::Result<Vec<FrameEntry>> {
        self.out.flush()?;
        self.out.get_ref().sync_all()?;
        Ok(self.frames)
    }
}

fn normalize_delay((numer, denom): (u32, u32)) -> u32 {
    let ms = numer.checked_div(denom).unwrap_or(0);
    if ms <= 10 { DEFAULT_DELAY_MS } else { ms.max(MIN_DELAY_MS) }
}

fn import_gif(path: &Path, w: &mut FrameWriter) -> Result<()> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let decoder = GifDecoder::new(BufReader::new(file))?;
    // Frames come out fully composited (disposal handled), one at a time.
    for frame in decoder.into_frames() {
        let frame = frame?;
        let delay = normalize_delay(frame.delay().numer_denom_ms());
        let jpeg = render::panel_jpeg(image::DynamicImage::ImageRgba8(frame.into_buffer()))?;
        w.push(&jpeg, u64::from(delay) * 1000)?;
    }
    Ok(())
}

fn write_entry(source: &Source, dir: &Path) -> Result<()> {
    fs::create_dir_all(dir)?;
    let mut w = FrameWriter::create(&dir.join(FRAMES_FILE))?;
    match source {
        Source::Color { rgb } => w.push(&render::solid_color(*rgb)?, 0)?,
        Source::Image { path } => w.push(&render::image_file(path)?, 0)?,
        Source::Gif { path } => import_gif(path, &mut w)?,
        Source::Video { path } => video::import(path, |jpeg, us| Ok(w.push(jpeg, us)?))?,
    }
    let frames = w.finish()?;
    anyhow::ensure!(!frames.is_empty(), "source has no frames");
    let index = Index { version: CACHE_VERSION, frames };
    fs::write(dir.join(INDEX_FILE), serde_json::to_vec(&index)?)?;
    Ok(())
}

/// Returns the cached frames for `source`, importing it first if needed.
pub fn import(source: &Source, cache_root: &Path) -> Result<CachedMedia> {
    let key = cache_key(source).context("reading source")?;
    let dir = cache_root.join(&key);
    if let Ok(media) = CachedMedia::open(&dir) {
        info!(key, frames = media.frames().len(), "using cached frames");
        return Ok(media);
    }

    // Build in a temp dir and rename, so a crash never leaves a half entry.
    let tmp = cache_root.join(format!("{key}.tmp{}", std::process::id()));
    let _ = fs::remove_dir_all(&tmp);
    let started = std::time::Instant::now();
    if let Err(e) = write_entry(source, &tmp) {
        let _ = fs::remove_dir_all(&tmp);
        return Err(e);
    }
    let _ = fs::remove_dir_all(&dir); // stale or corrupt entry
    fs::rename(&tmp, &dir).with_context(|| format!("moving cache entry to {}", dir.display()))?;

    let media = CachedMedia::open(&dir)?;
    info!(key, frames = media.frames().len(), elapsed = ?started.elapsed(), "imported");
    Ok(media)
}

/// Opens an existing cache entry by key without reading the source again.
pub fn open_cached(cache_root: &Path, key: &str) -> io::Result<CachedMedia> {
    if key.len() != 16 || !key.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid cache key"));
    }
    CachedMedia::open(&cache_root.join(key))
}

/// Deletes every cache entry except `keep`. Best effort: an entry still open
/// elsewhere may survive until the next prune.
pub fn prune(cache_root: &Path, keep: &str) {
    let Ok(entries) = fs::read_dir(cache_root) else { return };
    for entry in entries.flatten() {
        if entry.file_name() == keep || !entry.path().is_dir() {
            continue;
        }
        match fs::remove_dir_all(entry.path()) {
            Ok(()) => tracing::debug!(entry = ?entry.file_name(), "pruned cache entry"),
            Err(e) => tracing::debug!(entry = ?entry.file_name(), "could not prune: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::codecs::gif::{GifEncoder, Repeat};
    use image::{Delay, Frame, Rgba, RgbaImage};

    fn write_gif(path: &Path, frames: &[([u8; 3], u32)]) {
        let mut enc = GifEncoder::new(File::create(path).unwrap());
        enc.set_repeat(Repeat::Infinite).unwrap();
        for &(rgb, ms) in frames {
            let img = RgbaImage::from_pixel(40, 40, Rgba([rgb[0], rgb[1], rgb[2], 255]));
            enc.encode_frame(Frame::from_parts(img, 0, 0, Delay::from_numer_denom_ms(ms, 1)))
                .unwrap();
        }
    }

    fn center(media: &mut CachedMedia, i: usize) -> [u8; 3] {
        let mut buf = Vec::new();
        media.read_frame(i, &mut buf).unwrap();
        let img = image::load_from_memory(&buf).unwrap().to_rgb8();
        assert_eq!(img.dimensions(), (480, 480));
        img.get_pixel(240, 240).0
    }

    fn close(a: [u8; 3], b: [u8; 3]) -> bool {
        a.iter().zip(b).all(|(x, y)| x.abs_diff(y) <= 8)
    }

    #[test]
    fn imports_gif_frames_with_timing() {
        let tmp = tempfile::tempdir().unwrap();
        let gif = tmp.path().join("anim.gif");
        write_gif(&gif, &[([255, 0, 0], 50), ([0, 255, 0], 0), ([0, 0, 255], 120)]);

        let mut media = import(&Source::Gif { path: gif }, &tmp.path().join("cache")).unwrap();
        let durations: Vec<u64> = media.frames().iter().map(|f| f.duration_us).collect();
        assert_eq!(durations, [50_000, 100_000, 120_000]);
        assert!(!media.is_static());
        assert_eq!(media.loop_duration(), Duration::from_millis(270));
        assert!(close(center(&mut media, 0), [255, 0, 0]));
        assert!(close(center(&mut media, 1), [0, 255, 0]));
        assert!(close(center(&mut media, 2), [0, 0, 255]));
    }

    #[test]
    fn reuses_cache_and_reimports_on_change() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().join("cache");
        let gif = tmp.path().join("anim.gif");
        write_gif(&gif, &[([255, 0, 0], 50), ([0, 0, 255], 50)]);

        let first = import(&Source::Gif { path: gif.clone() }, &cache).unwrap();
        let mtime = fs::metadata(cache.join(first.key()).join(FRAMES_FILE)).unwrap().modified();
        let again = import(&Source::Gif { path: gif.clone() }, &cache).unwrap();
        assert_eq!(first.key(), again.key());
        let mtime2 = fs::metadata(cache.join(again.key()).join(FRAMES_FILE)).unwrap().modified();
        assert_eq!(mtime.unwrap(), mtime2.unwrap(), "cache entry was rewritten");

        write_gif(&gif, &[([0, 255, 0], 50), ([0, 0, 255], 50)]);
        let changed = import(&Source::Gif { path: gif }, &cache).unwrap();
        assert_ne!(first.key(), changed.key());
    }

    #[test]
    fn corrupt_entry_is_rebuilt() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().join("cache");
        let source = Source::Color { rgb: [0, 128, 128] };
        let key = import(&source, &cache).unwrap().key().to_owned();

        fs::write(cache.join(&key).join(FRAMES_FILE), b"truncated").unwrap();
        assert!(CachedMedia::open(&cache.join(&key)).is_err());
        let mut media = import(&source, &cache).unwrap();
        assert!(close(center(&mut media, 0), [0, 128, 128]));
    }

    #[test]
    fn color_and_image_are_static() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().join("cache");
        let png = tmp.path().join("still.png");
        RgbaImage::from_pixel(10, 10, Rgba([255, 255, 0, 255])).save(&png).unwrap();

        let mut color = import(&Source::Color { rgb: [255, 0, 255] }, &cache).unwrap();
        let mut still = import(&Source::Image { path: png }, &cache).unwrap();
        assert!(color.is_static() && still.is_static());
        assert!(close(center(&mut color, 0), [255, 0, 255]));
        assert!(close(center(&mut still, 0), [255, 255, 0]));
        // No temp dirs left behind.
        assert_eq!(fs::read_dir(&cache).unwrap().count(), 2);
    }

    #[test]
    fn single_frame_gif_is_static() {
        let tmp = tempfile::tempdir().unwrap();
        let gif = tmp.path().join("one.gif");
        write_gif(&gif, &[([1, 2, 3], 0)]);
        assert!(import(&Source::Gif { path: gif }, &tmp.path().join("c")).unwrap().is_static());
    }

    #[test]
    fn missing_file_fails_without_cache_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().join("cache");
        assert!(import(&Source::Gif { path: tmp.path().join("nope.gif") }, &cache).is_err());
        assert!(!cache.exists() || fs::read_dir(&cache).unwrap().count() == 0);
    }

    #[test]
    fn prune_keeps_only_current_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().join("cache");
        let red = import(&Source::Color { rgb: [255, 0, 0] }, &cache).unwrap().key().to_owned();
        import(&Source::Color { rgb: [0, 0, 255] }, &cache).unwrap();
        prune(&cache, &red);
        let left: Vec<_> = fs::read_dir(&cache).unwrap().flatten().map(|e| e.file_name()).collect();
        assert_eq!(left, [std::ffi::OsString::from(&red)]);
        assert!(open_cached(&cache, &red).is_ok());
    }

    #[test]
    fn open_cached_rejects_bad_keys() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(open_cached(tmp.path(), r"..\..\windows").is_err());
        assert!(open_cached(tmp.path(), "0123456789abcdef").is_err()); // missing
    }

    /// Needs ffmpeg on PATH (or AIO_FFMPEG); skipped otherwise.
    fn make_video(dir: &Path, rate: u32, seconds: f32) -> Option<std::path::PathBuf> {
        let tools = video::find_tools().ok()?;
        let path = dir.join(format!("clip{rate}.mp4"));
        let status = std::process::Command::new(tools.ffmpeg)
            .args(["-v", "error", "-y", "-f", "lavfi", "-i"])
            .arg(format!("testsrc2=size=320x240:rate={rate}:duration={seconds}"))
            .args(["-pix_fmt", "yuv420p"])
            .arg(&path)
            .status()
            .ok()?;
        status.success().then_some(path)
    }

    #[test]
    fn imports_video_at_source_rate() {
        let tmp = tempfile::tempdir().unwrap();
        let Some(clip) = make_video(tmp.path(), 25, 1.0) else {
            eprintln!("ffmpeg not available; skipping");
            return;
        };
        let mut media = import(&Source::Video { path: clip }, &tmp.path().join("cache")).unwrap();
        assert_eq!(media.frames().len(), 25);
        assert!(media.frames().iter().all(|f| f.duration_us == 40_000));
        assert_eq!(media.loop_duration(), Duration::from_secs(1));
        center(&mut media, 0); // decodes as a 480x480 JPEG
    }

    #[test]
    fn video_is_center_cropped() {
        let tmp = tempfile::tempdir().unwrap();
        let Ok(tools) = video::find_tools() else { return };
        // 320x240: a blue 240x240 square centered between red bars.
        let clip = tmp.path().join("bars.mp4");
        let ok = std::process::Command::new(tools.ffmpeg)
            .args(["-v", "error", "-y", "-f", "lavfi", "-i"])
            .arg("color=c=blue:s=240x240:r=10:d=0.5,pad=320:240:40:0:red")
            .args(["-pix_fmt", "yuv420p"])
            .arg(&clip)
            .status()
            .unwrap()
            .success();
        assert!(ok);
        let mut media = import(&Source::Video { path: clip }, &tmp.path().join("cache")).unwrap();
        let mut buf = Vec::new();
        media.read_frame(0, &mut buf).unwrap();
        let img = image::load_from_memory(&buf).unwrap().to_rgb8();
        for (x, y) in [(6, 6), (473, 6), (6, 473), (473, 473)] {
            let [r, _, b] = img.get_pixel(x, y).0;
            assert!(b > 180 && r < 70, "corner ({x},{y}) = {:?}", img.get_pixel(x, y).0);
        }
    }

    #[test]
    fn video_above_60fps_is_capped() {
        let tmp = tempfile::tempdir().unwrap();
        let Some(clip) = make_video(tmp.path(), 120, 0.5) else {
            eprintln!("ffmpeg not available; skipping");
            return;
        };
        let media = import(&Source::Video { path: clip }, &tmp.path().join("cache")).unwrap();
        assert_eq!(media.frames().len(), 30);
        assert_eq!(media.loop_duration(), Duration::from_millis(500));
    }

    #[test]
    fn broken_video_fails_cleanly() {
        let tmp = tempfile::tempdir().unwrap();
        if video::find_tools().is_err() {
            return;
        }
        let bogus = tmp.path().join("bogus.mp4");
        fs::write(&bogus, b"not a video").unwrap();
        let cache = tmp.path().join("cache");
        let err = import(&Source::Video { path: bogus }, &cache).err().unwrap();
        assert!(err.to_string().contains("ffmpeg failed"), "{err:#}");
        assert_eq!(fs::read_dir(&cache).unwrap().count(), 0, "temp entry left behind");
    }

    #[test]
    fn delays_follow_browser_rules() {
        assert_eq!(normalize_delay((0, 1)), 100);
        assert_eq!(normalize_delay((10, 1)), 100);
        assert_eq!(normalize_delay((15, 1)), 16);
        assert_eq!(normalize_delay((20, 1)), 20);
        assert_eq!(normalize_delay((70, 1)), 70);
        assert_eq!(normalize_delay((1000, 3)), 333);
    }
}
