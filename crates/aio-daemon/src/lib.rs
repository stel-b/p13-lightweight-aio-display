//! The aio daemon: device connection and playback, media import and frame
//! cache, config, IPC server and Windows service.

pub mod app;
pub mod config;
pub mod device;
pub mod hid;
pub mod media;
pub mod notify;
pub mod paths;
pub mod player;
pub mod render;
pub mod server;
pub mod service;
pub mod video;
pub mod winusb;
