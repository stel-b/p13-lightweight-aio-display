//! Finds a seamless loop in a video and cuts it out.
//!
//! You give an approximate start and end time. Frames are decoded (small) in
//! a window around both, every start frame is compared with every end frame
//! (including their neighbours, so motion has to continue too), and the best
//! pairs are offered. The loop is then cut as frames `[start, end)`: the
//! matching end frame itself is left out, because playback wraps to the
//! start frame, which looks the same.

pub mod crop;
pub mod ffmpeg;
pub mod matcher;
pub mod timecode;

use anyhow::{Result, bail};

use crate::ffmpeg::{Tools, VideoInfo};
use crate::matcher::{Candidate, Thumb};

/// Width of the frames decoded for comparing and previewing.
pub const PREVIEW_WIDTH: u32 = 160;
/// Largest search radius: keeps decoding and memory bounded.
pub const MAX_RADIUS: f64 = 5.0;

/// The outcome of a search, with the decoded frames for previews.
pub struct Search {
    pub preview_width: u32,
    pub preview_height: u32,
    /// Frame index of `start_frames[0]` / `end_frames[0]`.
    pub start0: u64,
    pub end0: u64,
    pub start_frames: Vec<Vec<u8>>,
    pub end_frames: Vec<Vec<u8>>,
    pub candidates: Vec<Candidate>,
}

impl Search {
    /// The decoded preview of frame `n`, if it was in either window.
    pub fn frame(&self, n: u64) -> Option<&[u8]> {
        fn get(frames: &[Vec<u8>], first: u64, n: u64) -> Option<&[u8]> {
            n.checked_sub(first).and_then(|i| frames.get(i as usize)).map(Vec::as_slice)
        }
        get(&self.start_frames, self.start0, n).or_else(|| get(&self.end_frames, self.end0, n))
    }
}

#[derive(Debug, Clone, Copy)]
pub enum Stage {
    DecodingStart,
    DecodingEnd,
    Comparing,
}

/// Looks for loops starting within `radius` seconds of `start` and ending
/// within `radius` seconds of `end`.
pub fn search(
    tools: &Tools,
    info: &VideoInfo,
    start: f64,
    end: f64,
    radius: f64,
    mut stage: impl FnMut(Stage),
) -> Result<Search> {
    let radius = radius.clamp(0.1, MAX_RADIUS);
    if !(0.0..=info.duration).contains(&start) || !(0.0..=info.duration).contains(&end) {
        bail!("times must be within the video (0 to {:.3} s)", info.duration);
    }
    if end <= start {
        bail!("the end must come after the start");
    }
    let rate = info.rate;
    let r = rate.frame_at(radius);
    let last = info.frames.saturating_sub(1);
    let window = |center: f64| {
        let c = rate.frame_at(center).min(last);
        let first = c.saturating_sub(r);
        (first, (c + r).min(last) - first + 1)
    };
    let (start0, start_len) = window(start);
    let (end0, end_len) = window(end);

    let width = PREVIEW_WIDTH;
    let height = info.preview_height(width);
    stage(Stage::DecodingStart);
    let start_frames = ffmpeg::decode_frames(tools, info, start0, start_len, width)?;
    stage(Stage::DecodingEnd);
    let end_frames = ffmpeg::decode_frames(tools, info, end0, end_len, width)?;
    stage(Stage::Comparing);
    let thumbs = |frames: &[Vec<u8>]| -> Vec<Thumb> {
        frames.iter().map(|f| Thumb::from_rgb(f, width as usize, height as usize)).collect()
    };
    let min_len = rate.frame_at(0.25).max(1);
    let spread = rate.frame_at(0.2).max(2);
    let candidates = matcher::find(&thumbs(&start_frames), start0, &thumbs(&end_frames), end0, min_len, 8, spread);
    if candidates.is_empty() {
        bail!("no loop possible in these windows (is the end too close to the start?)");
    }
    Ok(Search { preview_width: width, preview_height: height, start0, end0, start_frames, end_frames, candidates })
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

    pub fn fps(self) -> f64 {
        f64::from(self.num) / f64::from(self.den)
    }

    /// Start time of frame `n`, in seconds.
    pub fn time_of(self, n: u64) -> f64 {
        n as f64 * f64::from(self.den) / f64::from(self.num)
    }

    /// The frame shown at `seconds` (rounded to the nearest frame).
    pub fn frame_at(self, seconds: f64) -> u64 {
        (seconds.max(0.0) * self.fps()).round() as u64
    }
}
