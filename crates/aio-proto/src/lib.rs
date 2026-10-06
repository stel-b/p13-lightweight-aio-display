//! USB protocol for the 480×480 LCD on the MSI MPG CoreLiquid P13 AIO cooler.
//!
//! This crate has no USB dependency. Everything runs against the [`Transport`]
//! trait, so the protocol logic can be tested with [`mock::MockDevice`].
//!
//! Only the requests and bulk messages documented here are ever sent. Unknown
//! commands could put the LCD controller into an unrecoverable mode.

pub mod device_info;
pub mod display;
pub mod handshake;
pub mod header;
pub mod hid;
pub mod transport;

#[cfg(any(test, feature = "mock"))]
pub mod mock;

pub use device_info::DeviceInfo;
pub use display::{Display, Error, Step};
pub use handshake::{Auth, AuthResponses, DeviceKey, HandshakeData};
pub use header::{Header, Kind};
pub use transport::{Transport, TransportError, VendorRequest};

/// USB vendor ID (ArtInChip Technology).
pub const VENDOR_ID: u16 = 0x33C3;
/// USB product ID of the P13 LCD.
pub const PRODUCT_ID: u16 = 0x0E02;
/// The vendor-class display interface (handshake and frames).
pub const INTERFACE: u8 = 0;
/// The HID control interface (brightness, rotation; see [`hid`]).
pub const HID_INTERFACE: u8 = 1;
/// Bulk OUT endpoint: headers, handshake data and JPEG frames.
pub const EP_OUT: u8 = 0x01;
/// Bulk IN endpoint: handshake responses.
pub const EP_IN: u8 = 0x81;
/// Max packet size of both bulk endpoints (USB 2.0 high speed).
pub const MAX_PACKET_SIZE: usize = 512;

/// Panel width in pixels.
pub const WIDTH: u16 = 480;
/// Panel height in pixels.
pub const HEIGHT: u16 = 480;
