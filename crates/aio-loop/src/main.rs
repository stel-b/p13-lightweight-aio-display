//! aio-loop: find a seamless loop in a video and save it as `<name>_loop.mp4`.
//!
//! This is the window; `aio-loop-cli` does the same from a terminal.
//! `aio-loop <video>` opens the window with that video loaded.

#![windows_subsystem = "windows"]

mod gui;

fn main() {
    let video = std::env::args_os().nth(1).map(std::path::PathBuf::from);
    if let Err(e) = gui::run(video) {
        eprintln!("{e}");
    }
}
