//! A hidden message-only window that receives Windows device and power
//! notifications and turns them into device-thread commands:
//!
//! - display interface arrived → [`Command::DeviceArrived`] (reconnect now);
//! - display removed, or system resumed from sleep → [`Command::Reconnect`];
//! - system about to sleep → [`Command::FadeOut`], answered once the display
//!   has faded to black;
//! - Windows asks to disable/remove the device (`DBT_DEVICEQUERYREMOVE` on our
//!   handle) → [`Command::Release`], and the answer to Windows waits until the
//!   handle is closed, so the request succeeds instead of needing a reboot.
//!
//! Works the same as a service and in the foreground.

use std::cell::RefCell;
use std::ptr::{addr_of, null, null_mut};
use std::sync::mpsc::{self, Sender};
use std::time::Duration;

use tracing::{debug, info, warn};
use windows_sys::Win32::Foundation::{HANDLE, HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Power::RegisterSuspendResumeNotification;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DBT_DEVICEARRIVAL, DBT_DEVICEQUERYREMOVE, DBT_DEVICEREMOVECOMPLETE,
    DBT_DEVICEREMOVEPENDING, DBT_DEVTYP_DEVICEINTERFACE, DBT_DEVTYP_HANDLE,
    DEV_BROADCAST_DEVICEINTERFACE_W, DEV_BROADCAST_HANDLE, DEV_BROADCAST_HDR,
    DEVICE_NOTIFY_ALL_INTERFACE_CLASSES, DEVICE_NOTIFY_WINDOW_HANDLE, DefWindowProcW,
    DispatchMessageW, GetMessageW, HDEVNOTIFY, HWND_MESSAGE, MSG, PBT_APMRESUMEAUTOMATIC, PBT_APMSUSPEND,
    RegisterClassExW, RegisterDeviceNotificationW, TranslateMessage, UnregisterDeviceNotification,
    WM_DEVICECHANGE, WM_POWERBROADCAST, WNDCLASSEXW,
};

use crate::device::Command;
use crate::winusb::is_display_interface_path;

/// How long the window waits for the device thread to close the handle.
const RELEASE_TIMEOUT: Duration = Duration::from_secs(5);
/// Windows allows about 2 s to handle a suspend notification.
const FADE_TIMEOUT: Duration = Duration::from_millis(1800);

/// The notification window, for registering device handles with it.
#[derive(Debug, Clone, Copy)]
pub struct NotifyTarget(isize);

impl NotifyTarget {
    /// Registers an open device handle so Windows asks us before removing the
    /// device. Returns `None` (and logs) if registration fails.
    pub fn register_handle(self, handle: HANDLE) -> Option<HDEVNOTIFY> {
        let filter = DEV_BROADCAST_HANDLE {
            dbch_size: size_of::<DEV_BROADCAST_HANDLE>() as u32,
            dbch_devicetype: DBT_DEVTYP_HANDLE,
            dbch_handle: handle,
            ..Default::default()
        };
        // SAFETY: filter is a valid DEV_BROADCAST_HANDLE; the window lives for the process.
        let n = unsafe {
            RegisterDeviceNotificationW(self.0 as HWND, (&filter as *const DEV_BROADCAST_HANDLE).cast(), DEVICE_NOTIFY_WINDOW_HANDLE)
        };
        if n.is_null() {
            warn!("could not register for device removal requests: {}", std::io::Error::last_os_error());
            None
        } else {
            Some(n)
        }
    }
}

thread_local! {
    static COMMANDS: RefCell<Option<Sender<Command>>> = const { RefCell::new(None) };
}

fn send(cmd: Command) -> bool {
    COMMANDS.with_borrow(|tx| tx.as_ref().is_some_and(|tx| tx.send(cmd).is_ok()))
}

/// Starts the window thread and returns once the window exists.
pub fn spawn(commands: Sender<Command>) -> anyhow::Result<NotifyTarget> {
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("devnotify".into())
        .spawn(move || window_thread(commands, ready_tx))?;
    let hwnd = ready_rx.recv()?.map_err(anyhow::Error::msg)?;
    Ok(NotifyTarget(hwnd))
}

fn window_thread(commands: Sender<Command>, ready: mpsc::SyncSender<Result<isize, String>>) {
    COMMANDS.with_borrow_mut(|tx| *tx = Some(commands));
    let class: Vec<u16> = "aio-daemon-notify\0".encode_utf16().collect();
    let fail = |what: &str| {
        let _ = ready.send(Err(format!("{what}: {}", std::io::Error::last_os_error())));
    };
    // SAFETY: plain Win32 window setup on this thread; all pointers outlive the calls.
    unsafe {
        let hinstance = GetModuleHandleW(null());
        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wndproc),
            hInstance: hinstance,
            lpszClassName: class.as_ptr(),
            ..std::mem::zeroed()
        };
        if RegisterClassExW(&wc) == 0 {
            return fail("RegisterClassExW");
        }
        let hwnd = CreateWindowExW(0, class.as_ptr(), null(), 0, 0, 0, 0, 0, HWND_MESSAGE, null_mut(), hinstance, null());
        if hwnd.is_null() {
            return fail("CreateWindowExW");
        }
        let filter = DEV_BROADCAST_DEVICEINTERFACE_W {
            dbcc_size: size_of::<DEV_BROADCAST_DEVICEINTERFACE_W>() as u32,
            dbcc_devicetype: DBT_DEVTYP_DEVICEINTERFACE,
            ..Default::default()
        };
        let flags = DEVICE_NOTIFY_WINDOW_HANDLE | DEVICE_NOTIFY_ALL_INTERFACE_CLASSES;
        if RegisterDeviceNotificationW(hwnd, (&filter as *const DEV_BROADCAST_DEVICEINTERFACE_W).cast(), flags).is_null() {
            return fail("RegisterDeviceNotificationW");
        }
        if RegisterSuspendResumeNotification(hwnd, DEVICE_NOTIFY_WINDOW_HANDLE) == 0 {
            warn!("no resume notifications: {}", std::io::Error::last_os_error());
        }
        let _ = ready.send(Ok(hwnd as isize));

        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_DEVICECHANGE => {
            // SAFETY: for WM_DEVICECHANGE, lparam is null or a DEV_BROADCAST_* from Windows.
            unsafe { on_device_change(wparam as u32, lparam) };
            1 // TRUE: grant any removal request
        }
        WM_POWERBROADCAST => {
            match wparam as u32 {
                PBT_APMSUSPEND => {
                    info!("system going to sleep; fading out");
                    let (ack_tx, ack_rx) = mpsc::sync_channel(1);
                    if send(Command::FadeOut(ack_tx)) && ack_rx.recv_timeout(FADE_TIMEOUT).is_err() {
                        warn!("fade-out did not finish before sleep");
                    }
                }
                PBT_APMRESUMEAUTOMATIC => {
                    info!("system resumed; reconnecting");
                    send(Command::Reconnect("system resumed from sleep"));
                }
                _ => {}
            }
            1
        }
        // SAFETY: forwarding the original arguments.
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

/// The device path in a DEV_BROADCAST_DEVICEINTERFACE_W.
unsafe fn interface_name(p: *const DEV_BROADCAST_DEVICEINTERFACE_W) -> String {
    // SAFETY: dbcc_name is a NUL-terminated string that extends past the struct.
    unsafe {
        let start = addr_of!((*p).dbcc_name).cast::<u16>();
        let mut len = 0;
        while *start.add(len) != 0 {
            len += 1;
        }
        String::from_utf16_lossy(std::slice::from_raw_parts(start, len))
    }
}

unsafe fn on_device_change(event: u32, lparam: LPARAM) {
    if lparam == 0 {
        return;
    }
    // SAFETY: checked non-null; every DEV_BROADCAST_* starts with this header.
    let devtype = unsafe { (*(lparam as *const DEV_BROADCAST_HDR)).dbch_devicetype };
    match (event, devtype) {
        (DBT_DEVICEARRIVAL | DBT_DEVICEREMOVECOMPLETE, DBT_DEVTYP_DEVICEINTERFACE) => {
            // SAFETY: devtype says this is a DEV_BROADCAST_DEVICEINTERFACE_W.
            let name = unsafe { interface_name(lparam as *const DEV_BROADCAST_DEVICEINTERFACE_W) };
            if !is_display_interface_path(&name) {
                return;
            }
            if event == DBT_DEVICEARRIVAL {
                debug!("display interface arrived");
                send(Command::DeviceArrived);
            } else {
                send(Command::Reconnect("device removed"));
            }
        }
        (DBT_DEVICEQUERYREMOVE, DBT_DEVTYP_HANDLE) => {
            // Only our device handle is registered, so this is the display.
            // SAFETY: devtype says this is a DEV_BROADCAST_HANDLE.
            let notification = unsafe { (*(lparam as *const DEV_BROADCAST_HANDLE)).dbch_hdevnotify };
            info!("Windows requested the display (disable or removal); releasing it");
            let (ack_tx, ack_rx) = mpsc::sync_channel(1);
            if send(Command::Release(ack_tx)) && ack_rx.recv_timeout(RELEASE_TIMEOUT).is_err() {
                warn!("device thread did not release the display in time");
            }
            // The device thread skips this when releasing; it must happen here.
            // SAFETY: the registration belongs to the handle that was just closed.
            unsafe { UnregisterDeviceNotification(notification) };
        }
        (DBT_DEVICEREMOVEPENDING | DBT_DEVICEREMOVECOMPLETE, DBT_DEVTYP_HANDLE) => {
            send(Command::Reconnect("device removed"));
        }
        _ => {}
    }
}
