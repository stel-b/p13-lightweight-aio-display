//! The tray icon, as its own small process with no graphics stack: a plain
//! Win32 message loop. The settings window is a separate process started on
//! demand, so its memory is only used while it is open.

use aio_ipc::{Client, Request, Response};
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use windows_sys::Win32::UI::WindowsAndMessaging::{DispatchMessageW, GetMessageW, MSG, TranslateMessage};

/// A simple round icon in the cooler's style: a ring with a lit center.
pub fn icon_rgba(size: u32) -> Vec<u8> {
    let c = (size as f32 - 1.0) / 2.0;
    let mut rgba = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let d = ((x as f32 - c).powi(2) + (y as f32 - c).powi(2)).sqrt() / c;
            let (rgb, a) = if d <= 0.55 {
                ([0x2E, 0xC4, 0xB6], 255) // screen
            } else if d <= 0.95 {
                ([0x26, 0x2B, 0x33], 255) // pump ring
            } else if d <= 1.0 {
                ([0x26, 0x2B, 0x33], ((1.0 - d) / 0.05 * 255.0) as u8) // soft edge
            } else {
                ([0, 0, 0], 0)
            };
            rgba.extend_from_slice(&[rgb[0], rgb[1], rgb[2], a]);
        }
    }
    rgba
}

fn open_window() {
    if crate::focus_window() {
        return;
    }
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::process::Command::new(exe).spawn();
    }
}

/// Asks the daemon whether it is paused and flips it (off the UI thread).
fn toggle_pause() {
    std::thread::spawn(|| {
        let Ok(mut client) = Client::connect() else { return };
        if let Ok(Response::Status(s)) = client.request(&Request::GetStatus) {
            let _ = client.request(if s.paused { &Request::Resume } else { &Request::Pause });
        }
    });
}

pub fn run() -> anyhow::Result<()> {
    let open = MenuItem::new("Open AIO Display", true, None);
    let pause = MenuItem::new("Pause / resume", true, None);
    let quit = MenuItem::new("Remove tray icon", true, None);
    let menu = Menu::new();
    menu.append_items(&[&open, &pause, &PredefinedMenuItem::separator(), &quit])?;
    let _icon = TrayIconBuilder::new()
        .with_icon(Icon::from_rgba(icon_rgba(32), 32, 32)?)
        .with_tooltip(crate::WINDOW_TITLE)
        .with_menu(Box::new(menu))
        .with_menu_on_left_click(false)
        .build()?;

    // Tray and menu events are produced while messages are dispatched.
    let mut msg: MSG = unsafe { std::mem::zeroed() };
    // SAFETY: standard message loop on the thread that created the icon.
    while unsafe { GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) } > 0 {
        unsafe {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        while let Ok(event) = TrayIconEvent::receiver().try_recv() {
            if matches!(
                event,
                TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. }
                    | TrayIconEvent::DoubleClick { .. }
            ) {
                open_window();
            }
        }
        while let Ok(event) = MenuEvent::receiver().try_recv() {
            if event.id == *open.id() {
                open_window();
            } else if event.id == *pause.id() {
                toggle_pause();
            } else if event.id == *quit.id() {
                return Ok(());
            }
        }
    }
    Ok(())
}
