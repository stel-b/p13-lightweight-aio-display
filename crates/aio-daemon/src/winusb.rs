//! [`Transport`] over WinUSB, straight on the Win32 API.
//!
//! Owning the file handle lets the daemon register it for PnP notifications
//! and close it when Windows asks to disable or remove the device (see
//! [`crate::notify`]). Only interface 0 is opened; nothing ever touches the
//! HID interface or the device configuration.

use std::ffi::c_void;
use std::io;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::ptr::{null, null_mut};
use std::time::Duration;

use aio_proto::{EP_IN, EP_OUT, PRODUCT_ID, Transport, TransportError, VENDOR_ID, VendorRequest};
use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    CM_GET_DEVICE_INTERFACE_LIST_PRESENT, CM_GETIDLIST_FILTER_ENUMERATOR, CM_GETIDLIST_FILTER_PRESENT,
    CM_Get_Device_ID_List_SizeW, CM_Get_Device_ID_ListW, CM_Get_Device_Interface_List_SizeW,
    CM_Get_Device_Interface_ListW, CM_LOCATE_DEVNODE_NORMAL, CM_Locate_DevNodeW, CM_Open_DevNode_Key,
    CM_REGISTRY_HARDWARE, CR_BUFFER_SMALL, CR_SUCCESS, RegDisposition_OpenExisting,
};
use windows_sys::Win32::Devices::Usb::{
    PIPE_TRANSFER_TIMEOUT, WINUSB_INTERFACE_HANDLE, WINUSB_SETUP_PACKET, WinUsb_ControlTransfer,
    WinUsb_Free, WinUsb_Initialize, WinUsb_ReadPipe, WinUsb_SetPipePolicy, WinUsb_WritePipe,
};
use windows_sys::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_BAD_COMMAND, ERROR_DEVICE_NOT_CONNECTED, ERROR_FILE_NOT_FOUND,
    ERROR_GEN_FAILURE, ERROR_INVALID_HANDLE, ERROR_NO_SUCH_DEVICE, ERROR_SEM_TIMEOUT,
    ERROR_SHARING_VIOLATION, GENERIC_READ, GENERIC_WRITE, GetLastError, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_OVERLAPPED, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::Registry::{HKEY, KEY_READ, REG_MULTI_SZ, REG_SZ, RegCloseKey, RegQueryValueExW};
use windows_sys::Win32::UI::WindowsAndMessaging::{HDEVNOTIFY, UnregisterDeviceNotification};
use windows_sys::core::GUID;

use crate::notify::NotifyTarget;

#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    #[error("device {VENDOR_ID:04x}:{PRODUCT_ID:04x} interface 0 not found")]
    NotFound,
    /// Another program (e.g. Steam) holds the WinUSB handle; retry later.
    #[error("device is in use by another program: {0}")]
    Busy(String),
    #[error("{0}")]
    Other(String),
}

pub(crate) fn os_error(code: u32) -> String {
    io::Error::from_raw_os_error(code as i32).to_string()
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

/// Splits a double-NUL-terminated list of wide strings.
pub(crate) fn split_multi_sz(buf: &[u16]) -> Vec<String> {
    buf.split(|&c| c == 0).take_while(|s| !s.is_empty()).map(String::from_utf16_lossy).collect()
}

/// `USB\VID_33C3&PID_0E02&MI_00\...`: the display interface's device instance.
pub fn is_display_instance_id(id: &str) -> bool {
    id.to_ascii_uppercase().starts_with(r"USB\VID_33C3&PID_0E02&MI_00\")
}

/// `\\?\USB#VID_33C3&PID_0E02&MI_00#...#{guid}`: a device interface path of it.
pub fn is_display_interface_path(path: &str) -> bool {
    path.to_ascii_uppercase().contains("#VID_33C3&PID_0E02&MI_00#")
}

/// Parses `{xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx}`.
fn parse_guid(s: &str) -> Option<GUID> {
    let hex: String = s.trim().trim_start_matches('{').trim_end_matches('}').replace('-', "");
    if hex.len() != 32 {
        return None;
    }
    u128::from_str_radix(&hex, 16).ok().map(GUID::from_u128)
}

fn find_instance_id() -> Result<String, OpenError> {
    let filter = wide("USB");
    let flags = CM_GETIDLIST_FILTER_ENUMERATOR | CM_GETIDLIST_FILTER_PRESENT;
    loop {
        let mut len = 0;
        // SAFETY: valid out pointer and NUL-terminated filter.
        if unsafe { CM_Get_Device_ID_List_SizeW(&mut len, filter.as_ptr(), flags) } != CR_SUCCESS {
            return Err(OpenError::Other("listing USB devices failed".into()));
        }
        let mut buf = vec![0u16; len as usize];
        // SAFETY: buffer has `len` elements.
        match unsafe { CM_Get_Device_ID_ListW(filter.as_ptr(), buf.as_mut_ptr(), len, flags) } {
            CR_SUCCESS => {
                return split_multi_sz(&buf)
                    .into_iter()
                    .find(|id| is_display_instance_id(id))
                    .ok_or(OpenError::NotFound);
            }
            CR_BUFFER_SMALL => continue, // a device appeared in between
            cr => return Err(OpenError::Other(format!("listing USB devices failed ({cr})"))),
        }
    }
}

/// The interface GUID the WinUSB INF (Zadig) registered for the device.
fn interface_guid(instance_id: &[u16]) -> Result<GUID, OpenError> {
    let mut devinst = 0;
    // SAFETY: valid out pointer and NUL-terminated ID.
    if unsafe { CM_Locate_DevNodeW(&mut devinst, instance_id.as_ptr(), CM_LOCATE_DEVNODE_NORMAL) } != CR_SUCCESS {
        return Err(OpenError::NotFound);
    }
    let mut key: HKEY = null_mut();
    // SAFETY: valid out pointer.
    let cr = unsafe {
        CM_Open_DevNode_Key(devinst, KEY_READ, 0, RegDisposition_OpenExisting, &mut key, CM_REGISTRY_HARDWARE)
    };
    if cr != CR_SUCCESS {
        return Err(OpenError::Other(format!("opening device registry key failed ({cr})")));
    }
    let guid = ["DeviceInterfaceGUIDs", "DeviceInterfaceGUID"].iter().find_map(|name| {
        let name = wide(name);
        let mut buf = [0u16; 512];
        let mut size = std::mem::size_of_val(&buf) as u32;
        let mut ty = 0;
        // SAFETY: buffer and size describe the same memory.
        let r = unsafe { RegQueryValueExW(key, name.as_ptr(), null(), &mut ty, buf.as_mut_ptr().cast(), &mut size) };
        if r != 0 || (ty != REG_SZ && ty != REG_MULTI_SZ) {
            return None;
        }
        split_multi_sz(&buf).first().and_then(|s| parse_guid(s))
    });
    // SAFETY: key was opened above.
    unsafe { RegCloseKey(key) };
    guid.ok_or_else(|| {
        OpenError::Other("no WinUSB interface GUID; is WinUSB installed on \"P13 (Interface 0)\"?".into())
    })
}

fn interface_path(guid: &GUID, instance_id: &[u16]) -> Result<Vec<u16>, OpenError> {
    let mut len = 0;
    // SAFETY: valid pointers.
    let cr = unsafe {
        CM_Get_Device_Interface_List_SizeW(&mut len, guid, instance_id.as_ptr(), CM_GET_DEVICE_INTERFACE_LIST_PRESENT)
    };
    if cr != CR_SUCCESS || len <= 1 {
        return Err(OpenError::NotFound);
    }
    let mut buf = vec![0u16; len as usize];
    // SAFETY: buffer has `len` elements.
    let cr = unsafe {
        CM_Get_Device_Interface_ListW(guid, instance_id.as_ptr(), buf.as_mut_ptr(), len, CM_GET_DEVICE_INTERFACE_LIST_PRESENT)
    };
    if cr != CR_SUCCESS {
        return Err(OpenError::NotFound);
    }
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    if end == 0 {
        return Err(OpenError::NotFound);
    }
    buf.truncate(end);
    buf.push(0);
    Ok(buf)
}

fn last_transfer_error() -> TransportError {
    // SAFETY: trivially safe.
    match unsafe { GetLastError() } {
        ERROR_SEM_TIMEOUT => TransportError::Timeout,
        ERROR_GEN_FAILURE => TransportError::Stall,
        ERROR_DEVICE_NOT_CONNECTED | ERROR_NO_SUCH_DEVICE | ERROR_BAD_COMMAND | ERROR_FILE_NOT_FOUND
        | ERROR_INVALID_HANDLE => TransportError::Disconnected,
        code => TransportError::Other(os_error(code)),
    }
}

/// Pipe ids as WinUSB numbers them: 0 is the control pipe.
const CONTROL_PIPE: u8 = 0;

pub struct WinUsbTransport {
    winusb: WINUSB_INTERFACE_HANDLE,
    /// Kept after `winusb` in drop order: WinUsb_Free must run first.
    file: OwnedHandle,
    /// Last timeout set per pipe (control, OUT, IN), to skip redundant calls.
    timeouts: [Option<Duration>; 3],
    notification: Option<HDEVNOTIFY>,
}

// SAFETY: the handles are only used from one thread at a time (&mut self).
unsafe impl Send for WinUsbTransport {}

impl WinUsbTransport {
    /// Opens interface 0 of the P13. With `notify`, the handle is registered so
    /// Windows can ask for it back before disabling or removing the device.
    pub fn open(notify: Option<NotifyTarget>) -> Result<Self, OpenError> {
        let id = wide(&find_instance_id()?);
        let guid = interface_guid(&id)?;
        let path = interface_path(&guid, &id)?;

        // SAFETY: NUL-terminated path; other arguments are plain flags.
        let raw = unsafe {
            CreateFileW(
                path.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                null(),
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED, // required by WinUSB; calls below are still synchronous
                null_mut(),
            )
        };
        if raw == INVALID_HANDLE_VALUE {
            // SAFETY: trivially safe.
            return Err(match unsafe { GetLastError() } {
                code @ (ERROR_ACCESS_DENIED | ERROR_SHARING_VIOLATION) => OpenError::Busy(os_error(code)),
                ERROR_FILE_NOT_FOUND | ERROR_NO_SUCH_DEVICE => OpenError::NotFound,
                code => OpenError::Other(format!("opening device: {}", os_error(code))),
            });
        }
        // SAFETY: CreateFileW returned a valid handle that we now own.
        let file = unsafe { OwnedHandle::from_raw_handle(raw) };

        let mut winusb = null_mut();
        // SAFETY: valid file handle and out pointer.
        if unsafe { WinUsb_Initialize(file.as_raw_handle(), &mut winusb) } == 0 {
            // SAFETY: trivially safe.
            let code = unsafe { GetLastError() };
            return Err(OpenError::Other(format!(
                "WinUsb_Initialize failed: {} (is WinUSB installed on \"P13 (Interface 0)\"?)",
                os_error(code)
            )));
        }
        let notification = notify.and_then(|n| n.register_handle(file.as_raw_handle()));
        Ok(Self { winusb, file, timeouts: [None; 3], notification })
    }

    /// Called when the notification window already unregistered our handle
    /// (it does so itself while answering Windows' removal request).
    pub fn forget_notification(&mut self) {
        self.notification = None;
    }

    fn set_timeout(&mut self, pipe: u8, timeout: Duration) -> Result<(), TransportError> {
        let slot = match pipe {
            CONTROL_PIPE => 0,
            EP_OUT => 1,
            _ => 2,
        };
        if self.timeouts[slot] == Some(timeout) {
            return Ok(());
        }
        let ms = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX).max(1);
        // SAFETY: value points at a u32 of the stated length.
        let ok = unsafe {
            WinUsb_SetPipePolicy(self.winusb, pipe, PIPE_TRANSFER_TIMEOUT, 4, (&ms as *const u32).cast::<c_void>())
        };
        if ok == 0 {
            return Err(last_transfer_error());
        }
        self.timeouts[slot] = Some(timeout);
        Ok(())
    }

    fn control(&mut self, request_type: u8, req: VendorRequest, buf: &mut [u8], timeout: Duration) -> Result<usize, TransportError> {
        self.set_timeout(CONTROL_PIPE, timeout)?;
        let len = u16::try_from(buf.len()).map_err(|_| TransportError::Other("control transfer too long".into()))?;
        let setup = WINUSB_SETUP_PACKET {
            RequestType: request_type,
            Request: req.request,
            Value: req.value,
            Index: req.index,
            Length: len,
        };
        let mut done = 0;
        // SAFETY: buffer valid for `len` bytes; NULL overlapped = synchronous.
        let ok = unsafe { WinUsb_ControlTransfer(self.winusb, setup, buf.as_mut_ptr(), len.into(), &mut done, null()) };
        if ok == 0 { Err(last_transfer_error()) } else { Ok(done as usize) }
    }
}

impl Drop for WinUsbTransport {
    fn drop(&mut self) {
        // SAFETY: handles were created in `open` and are released exactly once.
        unsafe {
            if let Some(n) = self.notification.take() {
                UnregisterDeviceNotification(n);
            }
            WinUsb_Free(self.winusb);
        }
        // `file` closes after this.
        let _ = &self.file;
    }
}

/// Vendor request, device recipient (bmRequestType 0xC0 / 0x40).
const VENDOR_IN: u8 = 0xC0;
const VENDOR_OUT: u8 = 0x40;

impl Transport for WinUsbTransport {
    fn control_in(&mut self, req: VendorRequest, length: u16, timeout: Duration) -> Result<Vec<u8>, TransportError> {
        let mut buf = vec![0u8; length.into()];
        let n = self.control(VENDOR_IN, req, &mut buf, timeout)?;
        buf.truncate(n);
        Ok(buf)
    }

    fn control_out(&mut self, req: VendorRequest, data: &[u8], timeout: Duration) -> Result<(), TransportError> {
        let mut buf = data.to_vec();
        self.control(VENDOR_OUT, req, &mut buf, timeout).map(|_| ())
    }

    fn bulk_write(&mut self, data: &[u8], timeout: Duration) -> Result<(), TransportError> {
        self.set_timeout(EP_OUT, timeout)?;
        let len = u32::try_from(data.len()).map_err(|_| TransportError::Other("write too long".into()))?;
        let mut done = 0;
        // SAFETY: buffer valid for `len` bytes; NULL overlapped = synchronous.
        if unsafe { WinUsb_WritePipe(self.winusb, EP_OUT, data.as_ptr(), len, &mut done, null()) } == 0 {
            return Err(last_transfer_error());
        }
        if done != len {
            return Err(TransportError::Other(format!("short write: {done} of {len} bytes")));
        }
        Ok(())
    }

    fn bulk_read(&mut self, max_len: usize, timeout: Duration) -> Result<Vec<u8>, TransportError> {
        self.set_timeout(EP_IN, timeout)?;
        let mut buf = vec![0u8; max_len];
        let mut done = 0;
        // SAFETY: buffer valid for `max_len` bytes; NULL overlapped = synchronous.
        if unsafe { WinUsb_ReadPipe(self.winusb, EP_IN, buf.as_mut_ptr(), max_len as u32, &mut done, null()) } == 0 {
            return Err(last_transfer_error());
        }
        buf.truncate(done as usize);
        Ok(buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_only_display_interface() {
        assert!(is_display_instance_id(r"USB\VID_33C3&PID_0E02&MI_00\7&2B1F3A&0&0000"));
        assert!(is_display_instance_id(r"usb\vid_33c3&pid_0e02&mi_00\x"));
        assert!(!is_display_instance_id(r"USB\VID_33C3&PID_0E02&MI_01\7&2B1F3A&0&0001")); // HID
        assert!(!is_display_instance_id(r"USB\VID_33C3&PID_0E02\0001")); // composite parent

        assert!(is_display_interface_path(
            r"\\?\USB#VID_33C3&PID_0E02&MI_00#7&2b1f3a&0&0000#{dee824ef-729b-4a0e-9c14-b7117d33a817}"
        ));
        assert!(!is_display_interface_path(r"\\?\HID#VID_33C3&PID_0E02&MI_01#8&1#{4d1e55b2}"));
    }

    #[test]
    fn parses_guids() {
        let g = parse_guid("{dee824ef-729b-4a0e-9c14-b7117d33a817}").unwrap();
        assert_eq!((g.data1, g.data2, g.data3), (0xdee824ef, 0x729b, 0x4a0e));
        assert_eq!(g.data4, [0x9c, 0x14, 0xb7, 0x11, 0x7d, 0x33, 0xa8, 0x17]);
        assert!(parse_guid("{nope}").is_none());
    }

    #[test]
    fn splits_wide_lists() {
        let buf: Vec<u16> = "a\0bc\0\0junk".encode_utf16().collect();
        assert_eq!(split_multi_sz(&buf), ["a", "bc"]);
    }
}
