//! The 160 bytes returned by vendor control request 0.
//!
//! ```text
//!   0..16   info: width at 10, height at 12, refresh rate at 14 (u16 LE)
//!  16..144  EDID (monitor name "USB Display")
//! 144..160  trailer (byte 152 = 0xB4, possibly rotation; unconfirmed)
//! ```

pub const DEVICE_INFO_LEN: usize = 160;

#[derive(Debug, thiserror::Error)]
#[error("device info is {0} bytes, expected 160")]
pub struct DeviceInfoError(pub usize);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    pub raw_info: [u8; 16],
    pub width: u16,
    pub height: u16,
    pub refresh_hz: u16,
    pub edid: [u8; 128],
    pub trailer: [u8; 16],
}

impl DeviceInfo {
    pub fn parse(bytes: &[u8]) -> Result<Self, DeviceInfoError> {
        if bytes.len() != DEVICE_INFO_LEN {
            return Err(DeviceInfoError(bytes.len()));
        }
        let u16_at = |i: usize| u16::from_le_bytes([bytes[i], bytes[i + 1]]);
        Ok(Self {
            raw_info: bytes[0..16].try_into().unwrap(),
            width: u16_at(10),
            height: u16_at(12),
            refresh_hz: u16_at(14),
            edid: bytes[16..144].try_into().unwrap(),
            trailer: bytes[144..160].try_into().unwrap(),
        })
    }

    /// True if the reported resolution is the 480×480 panel we encode for.
    pub fn is_expected_panel(&self) -> bool {
        self.width == crate::WIDTH && self.height == crate::HEIGHT
    }

    pub fn edid_header_valid(&self) -> bool {
        self.edid[..8] == [0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00]
    }

    /// Monitor name from the EDID's display descriptors (tag 0xFC).
    pub fn monitor_name(&self) -> Option<String> {
        [54, 72, 90, 108].into_iter().find_map(|off| {
            let d = &self.edid[off..off + 18];
            if d[..3] != [0, 0, 0] || d[3] != 0xFC {
                return None;
            }
            let text = &d[5..];
            let end = text.iter().position(|&b| b == 0x0A).unwrap_or(text.len());
            Some(String::from_utf8_lossy(&text[..end]).trim_end().to_owned())
        })
    }
}

/// Device info shaped like the real device's (for tests and the mock).
#[cfg(any(test, feature = "mock"))]
pub fn sample_bytes() -> Vec<u8> {
    let mut b = vec![
        0x05, 0x00, 0x80, 0x00, 0x10, 0x00, 0x00, 0x00, 0x01, 0x00, 0xe0, 0x01, 0xe0, 0x01, 0x3c,
        0x00,
    ];
    let mut edid = [0u8; 128];
    edid[..8].copy_from_slice(&[0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00]);
    let name = &mut edid[72..90];
    name[3] = 0xFC;
    name[5..].copy_from_slice(b"USB Display\n ");
    b.extend(edid);
    let mut trailer = [0u8; 16];
    trailer[8] = 0xB4;
    b.extend(trailer);
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sample() {
        let info = DeviceInfo::parse(&sample_bytes()).unwrap();
        assert_eq!((info.width, info.height, info.refresh_hz), (480, 480, 60));
        assert!(info.is_expected_panel());
        assert!(info.edid_header_valid());
        assert_eq!(info.monitor_name().as_deref(), Some("USB Display"));
        assert_eq!(info.trailer[8], 0xB4);
    }

    #[test]
    fn rejects_wrong_length() {
        assert!(DeviceInfo::parse(&[0; 159]).is_err());
        assert!(DeviceInfo::parse(&[0; 161]).is_err());
    }

    #[test]
    fn detects_other_resolution() {
        let mut b = sample_bytes();
        b[10] = 0x40; // width 0x0140 = 320
        b[11] = 0x01;
        assert!(!DeviceInfo::parse(&b).unwrap().is_expected_panel());
    }
}
