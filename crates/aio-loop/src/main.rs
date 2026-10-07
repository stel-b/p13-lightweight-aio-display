//! aio-loop: find a seamless loop in a video, pick a square crop, and save it
//! as `<name>_loop.mp4`.
//!
//!   aio-loop [--for-display] [video]
//!
//! `--for-display` (used by AIO Display) turns the crop on, preset to what the
//! pump shows. `aio-loop-cli` does the same from a terminal.

#![windows_subsystem = "windows"]

mod gui;

fn main() {
    let mut for_display = false;
    let mut video = None;
    for arg in std::env::args_os().skip(1) {
        if arg == "--for-display" {
            for_display = true;
        } else {
            video = Some(std::path::PathBuf::from(arg));
        }
    }
    if let Err(e) = gui::run(video, for_display) {
        eprintln!("{e}");
    }
}
