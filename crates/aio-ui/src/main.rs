//! aio-ui: settings window and tray icon for the aio daemon.
//!
//!   aio-ui          the settings window; closing it exits (frees all memory)
//!   aio-ui --tray   tray icon only (no graphics); opens the window on demand
//!
//! The daemon keeps the display running whether or not either is open.

#![windows_subsystem = "windows"]

mod backend;
mod tray;
mod window;

/// Window title; also how the tray finds an open window.
pub const WINDOW_TITLE: &str = "AIO Display";

/// True if another process already holds the named mutex `name`.
pub fn already_running(name: &str) -> bool {
    use windows_sys::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError};
    use windows_sys::Win32::System::Threading::CreateMutexW;
    let wide: Vec<u16> = name.encode_utf16().chain([0]).collect();
    // SAFETY: valid NUL-terminated name; the handle is kept for the process lifetime.
    unsafe {
        let handle = CreateMutexW(std::ptr::null(), 0, wide.as_ptr());
        !handle.is_null() && GetLastError() == ERROR_ALREADY_EXISTS
    }
}

/// Brings an already open settings window to the front. False if none.
pub fn focus_window() -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::{FindWindowW, SW_RESTORE, SetForegroundWindow, ShowWindow};
    let title: Vec<u16> = WINDOW_TITLE.encode_utf16().chain([0]).collect();
    // SAFETY: valid NUL-terminated title; the handle is only used for these calls.
    unsafe {
        let hwnd = FindWindowW(std::ptr::null(), title.as_ptr());
        if hwnd.is_null() {
            return false;
        }
        ShowWindow(hwnd, SW_RESTORE);
        SetForegroundWindow(hwnd);
        true
    }
}

fn main() {
    if std::env::args().any(|a| a == "--tray") {
        if !already_running("Local\\aio-ui-tray")
            && let Err(e) = tray::run()
        {
            eprintln!("tray: {e:#}");
        }
    } else if already_running("Local\\aio-ui-window") {
        focus_window();
    } else if let Err(e) = window::run() {
        eprintln!("window: {e}");
    }
}
