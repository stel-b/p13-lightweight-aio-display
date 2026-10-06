use std::time::Duration;

/// A vendor control request. In this protocol every control request is
/// vendor-type with a device recipient (`bmRequestType` 0x40 OUT / 0xC0 IN).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VendorRequest {
    pub request: u8,
    pub value: u16,
    pub index: u16,
}

impl VendorRequest {
    pub const fn new(request: u8, value: u16, index: u16) -> Self {
        Self { request, value, index }
    }
}

/// Raw access to interface 0 of the device.
///
/// The bulk methods use [`crate::EP_OUT`] and [`crate::EP_IN`].
pub trait Transport {
    /// Vendor control IN transfer; returns the bytes the device sent.
    fn control_in(
        &mut self,
        req: VendorRequest,
        length: u16,
        timeout: Duration,
    ) -> Result<Vec<u8>, TransportError>;

    /// Vendor control OUT transfer.
    fn control_out(
        &mut self,
        req: VendorRequest,
        data: &[u8],
        timeout: Duration,
    ) -> Result<(), TransportError>;

    /// One bulk OUT transfer containing all of `data`.
    fn bulk_write(&mut self, data: &[u8], timeout: Duration) -> Result<(), TransportError>;

    /// One bulk IN transfer of up to `max_len` bytes (a short packet ends it).
    /// Must return [`TransportError::Timeout`] when nothing arrives in time.
    fn bulk_read(&mut self, max_len: usize, timeout: Duration) -> Result<Vec<u8>, TransportError>;

    /// Waits between protocol steps. Mocks override this to avoid real delays.
    fn sleep(&mut self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("transfer timed out")]
    Timeout,
    #[error("device disconnected")]
    Disconnected,
    #[error("endpoint stalled")]
    Stall,
    #[error("{0}")]
    Other(String),
}
