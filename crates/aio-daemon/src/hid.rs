//! [`HidTransport`] for the P13's HID interface (interface 1) through the
//! standard Windows HID driver. Access is shared, so no driver change is
//! needed and other software may have it open too.

use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::ptr::{null, null_mut};
use std::time::Duration;

use aio_proto::TransportError;
use aio_proto::hid::{HidTransport, REPORT_LEN};
use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    CM_GET_DEVICE_INTERFACE_LIST_PRESENT, CM_Get_Device_Interface_List_SizeW, CM_Get_Device_Interface_ListW,
    CR_BUFFER_SMALL, CR_SUCCESS,
};
use windows_sys::Win32::Devices::HumanInterfaceDevice::{
    GUID_DEVINTERFACE_HID, HIDP_CAPS, HIDP_STATUS_SUCCESS, HidD_FlushQueue, HidD_FreePreparsedData,
    HidD_GetPreparsedData, HidP_GetCaps,
};
use windows_sys::Win32::Foundation::{
    ERROR_DEVICE_NOT_CONNECTED, ERROR_FILE_NOT_FOUND, ERROR_GEN_FAILURE, ERROR_IO_PENDING, ERROR_NO_SUCH_DEVICE,
    ERROR_OPERATION_ABORTED, GENERIC_READ, GENERIC_WRITE, GetLastError, INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_OVERLAPPED, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING, ReadFile, WriteFile,
};
use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows_sys::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

use crate::winusb::{OpenError, os_error, split_multi_sz};

/// `\\?\HID#VID_33C3&PID_0E02&MI_01#...`: the P13's control interface.
pub fn is_p13_hid_path(path: &str) -> bool {
    path.to_ascii_uppercase().contains("#VID_33C3&PID_0E02&MI_01#")
}

/// Windows prefixes each report with its report ID (0 here).
const WIN_REPORT_LEN: usize = REPORT_LEN + 1;
const WRITE_TIMEOUT: Duration = Duration::from_secs(1);

fn find_path() -> Result<Vec<u16>, OpenError> {
    loop {
        let mut len = 0;
        // SAFETY: valid pointers; NULL device ID lists all devices.
        let cr = unsafe {
            CM_Get_Device_Interface_List_SizeW(&mut len, &GUID_DEVINTERFACE_HID, null(), CM_GET_DEVICE_INTERFACE_LIST_PRESENT)
        };
        if cr != CR_SUCCESS {
            return Err(OpenError::Other(format!("listing HID devices failed ({cr})")));
        }
        let mut buf = vec![0u16; len as usize];
        // SAFETY: buffer has `len` elements.
        let cr = unsafe {
            CM_Get_Device_Interface_ListW(&GUID_DEVINTERFACE_HID, null(), buf.as_mut_ptr(), len, CM_GET_DEVICE_INTERFACE_LIST_PRESENT)
        };
        match cr {
            CR_SUCCESS => {
                let path = split_multi_sz(&buf).into_iter().find(|p| is_p13_hid_path(p)).ok_or(OpenError::NotFound)?;
                return Ok(path.encode_utf16().chain([0]).collect());
            }
            CR_BUFFER_SMALL => continue,
            cr => return Err(OpenError::Other(format!("listing HID devices failed ({cr})"))),
        }
    }
}

fn last_error() -> TransportError {
    // SAFETY: trivially safe.
    match unsafe { GetLastError() } {
        ERROR_DEVICE_NOT_CONNECTED | ERROR_NO_SUCH_DEVICE | ERROR_FILE_NOT_FOUND | ERROR_GEN_FAILURE => {
            TransportError::Disconnected
        }
        code => TransportError::Other(os_error(code)),
    }
}

/// A manual-reset event plus OVERLAPPED for one I/O at a time.
struct Overlapped {
    event: OwnedHandle,
    ov: OVERLAPPED,
}

impl Overlapped {
    fn new() -> Result<Self, OpenError> {
        // SAFETY: plain event creation.
        let event = unsafe { CreateEventW(null(), 1, 0, null()) };
        if event.is_null() {
            return Err(OpenError::Other(format!("CreateEvent: {}", std::io::Error::last_os_error())));
        }
        // SAFETY: CreateEventW returned a valid handle we now own.
        let event = unsafe { OwnedHandle::from_raw_handle(event) };
        Ok(Self { event, ov: unsafe { std::mem::zeroed() } })
    }

    fn reset(&mut self) -> *mut OVERLAPPED {
        // SAFETY: OVERLAPPED is plain data; the event stays ours.
        self.ov = unsafe { std::mem::zeroed() };
        self.ov.hEvent = self.event.as_raw_handle();
        &mut self.ov
    }
}

pub struct HidDevice {
    file: OwnedHandle,
    read: Overlapped,
    write: Overlapped,
}

// SAFETY: handles are only used through &mut self, from one thread at a time.
unsafe impl Send for HidDevice {}

impl HidDevice {
    pub fn open() -> Result<Self, OpenError> {
        let path = find_path()?;
        // SAFETY: NUL-terminated path; plain flags.
        let raw = unsafe {
            CreateFileW(
                path.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                null(),
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED,
                null_mut(),
            )
        };
        if raw == INVALID_HANDLE_VALUE {
            // SAFETY: trivially safe.
            let code = unsafe { GetLastError() };
            return Err(OpenError::Other(format!("opening the HID interface: {}", os_error(code))));
        }
        // SAFETY: CreateFileW returned a valid handle we now own.
        let file = unsafe { OwnedHandle::from_raw_handle(raw) };
        check_report_sizes(&file)?;
        Ok(Self { file, read: Overlapped::new()?, write: Overlapped::new()? })
    }
}

/// Waits for the pending I/O on `which`, cancelling it after `timeout`.
fn finish(
    handle: windows_sys::Win32::Foundation::HANDLE,
    which: &Overlapped,
    timeout: Duration,
) -> Result<usize, TransportError> {
    let ms = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX);
    // SAFETY: the event and OVERLAPPED belong to the pending I/O.
    let waited = unsafe { WaitForSingleObject(which.event.as_raw_handle(), ms) };
    if waited != WAIT_OBJECT_0 {
        // SAFETY: cancels only our own pending I/O on this handle.
        unsafe { CancelIoEx(handle, &which.ov) };
    }
    let mut done = 0;
    // SAFETY: waits (bWait=TRUE) until the I/O, possibly cancelled, completes.
    let ok = unsafe { GetOverlappedResult(handle, &which.ov, &mut done, 1) };
    if ok == 0 {
        // SAFETY: trivially safe.
        return Err(match unsafe { GetLastError() } {
            ERROR_OPERATION_ABORTED => TransportError::Timeout,
            _ => last_error(),
        });
    }
    Ok(done as usize)
}

fn check_report_sizes(file: &OwnedHandle) -> Result<(), OpenError> {
    let mut data = 0;
    // SAFETY: valid handle and out pointer; freed below.
    if !unsafe { HidD_GetPreparsedData(file.as_raw_handle(), &mut data) } {
        return Err(OpenError::Other("HidD_GetPreparsedData failed".into()));
    }
    // SAFETY: `data` came from HidD_GetPreparsedData.
    let mut caps: HIDP_CAPS = unsafe { std::mem::zeroed() };
    let status = unsafe { HidP_GetCaps(data, &mut caps) };
    unsafe { HidD_FreePreparsedData(data) };
    if status != HIDP_STATUS_SUCCESS {
        return Err(OpenError::Other("HidP_GetCaps failed".into()));
    }
    let (input, output) = (usize::from(caps.InputReportByteLength), usize::from(caps.OutputReportByteLength));
    if input != WIN_REPORT_LEN || output != WIN_REPORT_LEN {
        return Err(OpenError::Other(format!(
            "unexpected HID report sizes (in {input}, out {output}); not the P13 control interface?"
        )));
    }
    Ok(())
}

impl HidTransport for HidDevice {
    fn write_report(&mut self, report: &[u8; REPORT_LEN]) -> Result<(), TransportError> {
        let mut buf = [0u8; WIN_REPORT_LEN]; // report ID 0, then the report
        buf[1..].copy_from_slice(report);
        let handle = self.file.as_raw_handle();
        let ov = self.write.reset();
        // SAFETY: `buf` and the OVERLAPPED outlive the I/O: `finish` waits for completion.
        let ok = unsafe { WriteFile(handle, buf.as_ptr(), buf.len() as u32, null_mut(), ov) };
        if ok == 0 && unsafe { GetLastError() } != ERROR_IO_PENDING {
            return Err(last_error());
        }
        match finish(handle, &self.write, WRITE_TIMEOUT)? {
            WIN_REPORT_LEN => Ok(()),
            n => Err(TransportError::Other(format!("short HID write ({n} bytes)"))),
        }
    }

    fn read_report(&mut self, timeout: Duration) -> Result<Vec<u8>, TransportError> {
        let mut buf = vec![0u8; WIN_REPORT_LEN];
        let handle = self.file.as_raw_handle();
        let ov = self.read.reset();
        // SAFETY: `buf` and the OVERLAPPED outlive the I/O: `finish` waits for completion.
        let ok = unsafe { ReadFile(handle, buf.as_mut_ptr(), buf.len() as u32, null_mut(), ov) };
        if ok == 0 && unsafe { GetLastError() } != ERROR_IO_PENDING {
            return Err(last_error());
        }
        let n = finish(handle, &self.read, timeout)?;
        Ok(buf[1..n.max(1)].to_vec()) // drop the report ID byte
    }

    fn flush_input(&mut self) -> Result<(), TransportError> {
        // SAFETY: valid HID handle.
        if unsafe { HidD_FlushQueue(self.file.as_raw_handle()) } { Ok(()) } else { Err(last_error()) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_only_the_control_interface() {
        assert!(is_p13_hid_path(r"\\?\HID#VID_33C3&PID_0E02&MI_01#8&1a2b3c4d&0&0000#{4d1e55b2-f16f-11cf-88cb-001111000030}"));
        assert!(!is_p13_hid_path(r"\\?\USB#VID_33C3&PID_0E02&MI_00#7&5e6f7a8b&0&0000#{dee824ef}"));
        assert!(!is_p13_hid_path(r"\\?\HID#VID_046D&PID_C52B&MI_01#7&1#{4d1e55b2}"));
    }
}
