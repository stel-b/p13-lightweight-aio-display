//! Real ffmpeg round trip on a generated video with a known loop period.
//! Skipped (passes) when ffmpeg is not installed.

use std::path::{Path, PathBuf};
use std::process::Command;

use aio_loop::crop::Crop;
use aio_loop::ffmpeg::{self, CutOptions, Tools};
use aio_loop::matcher::Thumb;
use aio_loop::{PREVIEW_WIDTH, search};

/// Frames per turn of the orbiting circle.
const PERIOD: u64 = 90;

/// 12 s at 30 fps: a bright circle orbiting with a 3-second period.
fn make_clip(tools: &Tools, dir: &Path) -> PathBuf {
    let path = dir.join("orbit.mp4");
    let source = "color=c=black:s=160x120:r=30:d=12,format=gray,\
                  geq=lum='if(lt(hypot(X-80-40*cos(2*PI*T/3),Y-60-40*sin(2*PI*T/3)),12),235,16)'";
    let ok = Command::new(&tools.ffmpeg)
        .args(["-v", "error", "-y", "-f", "lavfi", "-i", source, "-c:v", "libx264", "-pix_fmt", "yuv420p"])
        .arg(&path)
        .status()
        .expect("run ffmpeg")
        .success();
    assert!(ok, "could not generate the test clip");
    path
}

fn thumb(tools: &Tools, info: &ffmpeg::VideoInfo, n: u64) -> Thumb {
    let frame = &ffmpeg::decode_frames(tools, info, n, 1, PREVIEW_WIDTH).unwrap()[0];
    Thumb::from_rgb(frame, PREVIEW_WIDTH as usize, info.preview_height(PREVIEW_WIDTH) as usize)
}

#[test]
fn finds_and_cuts_a_seamless_loop() {
    let Ok(tools) = ffmpeg::find_tools() else {
        eprintln!("ffmpeg not available; skipping");
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let clip = make_clip(&tools, tmp.path());
    let info = ffmpeg::probe(&tools, &clip).unwrap();
    assert_eq!(info.rate.fps(), 30.0);
    assert_eq!(info.frames, 360);

    // Rough marks: the true loop length near this is 180 frames (6 s).
    let found = search(&tools, &info, 2.0, 8.1, 1.0, |_| {}).unwrap();
    let best = found.candidates[0];
    assert_eq!(best.frames() % PERIOD, 0, "{best:?}");
    assert!(best.score < 0.01, "{best:?}");
    assert!((50..=90).contains(&best.start), "start outside the 2 s +- 1 s window: {best:?}");

    let out = tmp.path().join("orbit_loop.mp4");
    ffmpeg::cut(&tools, &info, &CutOptions::frames(best.start, best.end), &out, |_| {}).unwrap();
    let cut = ffmpeg::probe(&tools, &out).unwrap();
    assert_eq!(cut.frames, best.frames(), "the loop must have exactly end - start frames");

    // The cut starts at `start`, ends at `end - 1`, and wraps seamlessly.
    let first = thumb(&tools, &cut, 0);
    let last = thumb(&tools, &cut, cut.frames - 1);
    assert!(first.distance(&thumb(&tools, &info, best.start)) < 0.01);
    assert!(last.distance(&thumb(&tools, &info, best.end - 1)) < 0.01);
    let seam = last.distance(&first);
    let normal_step = thumb(&tools, &info, best.start).distance(&thumb(&tools, &info, best.start + 1));
    assert!(seam <= normal_step * 1.5 + 0.002, "seam {seam} vs a normal frame step {normal_step}");
}

#[test]
fn cuts_a_cropped_scaled_square() {
    let Ok(tools) = ffmpeg::find_tools() else { return };
    let tmp = tempfile::tempdir().unwrap();
    let info = ffmpeg::probe(&tools, &make_clip(&tools, tmp.path())).unwrap();
    let out = tmp.path().join("square.mp4");
    let crop = Crop::around(80.0, 60.0, 100.0, info.width, info.height);
    let opts = CutOptions { crop: Some(crop), scale: Some(96), ..CutOptions::frames(30, 60) };
    ffmpeg::cut(&tools, &info, &opts, &out, |_| {}).unwrap();
    let cut = ffmpeg::probe(&tools, &out).unwrap();
    assert_eq!((cut.width, cut.height, cut.frames), (96, 96, 30));

    let too_big = CutOptions { crop: Some(Crop { x: 100, y: 0, size: 120 }), ..CutOptions::frames(0, 10) };
    assert!(ffmpeg::cut(&tools, &info, &too_big, &out, |_| {}).is_err(), "crop outside the frame");
}

#[test]
fn rejects_bad_ranges() {
    let Ok(tools) = ffmpeg::find_tools() else { return };
    let tmp = tempfile::tempdir().unwrap();
    let info = ffmpeg::probe(&tools, &make_clip(&tools, tmp.path())).unwrap();
    assert!(search(&tools, &info, 5.0, 4.0, 1.0, |_| {}).is_err(), "end before start");
    assert!(search(&tools, &info, 1.0, 99.0, 1.0, |_| {}).is_err(), "end past the video");
}
