//! The 20-byte header that precedes every bulk OUT message (except the final
//! 60-byte handshake block).
//!
//! ```text
//! 0x00 u32  magic = 0xA1C62B00 | kind
//! 0x04 u32  payload length
//! 0x08 u16  sequence number (frames only)
//! 0x0A u16  flags: 0x0010 for frames, 0 for auth messages (meaning unknown)
//! 0x0C u32  reserved, 0
//! 0x10 u32  magic repeated
//! ```

pub const HEADER_LEN: usize = 20;
pub const MAGIC_BASE: u32 = 0xA1C6_2B00;
/// Flags value MSI's driver uses for image frames.
pub const FLAGS_FRAME: u16 = 0x0010;

/// Message kinds seen in captures. No other kinds are ever sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Kind {
    /// Image frame; payload is a 480×480 baseline JPEG.
    Frame = 0x01,
    /// Auth step 1; payload is the 256-byte `blob1`.
    Auth1 = 0x10,
    /// Auth step 2; no payload, device answers with 256 bytes.
    Auth2 = 0x11,
}

impl Kind {
    pub fn from_u8(value: u8) -> Option<Self> {
        match value {
            0x01 => Some(Kind::Frame),
            0x10 => Some(Kind::Auth1),
            0x11 => Some(Kind::Auth2),
            _ => None,
        }
    }

    pub fn magic(self) -> u32 {
        MAGIC_BASE | self as u32
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub kind: Kind,
    pub length: u32,
    pub seq: u16,
    pub flags: u16,
}

impl Header {
    pub fn auth1(length: u32) -> Self {
        Self { kind: Kind::Auth1, length, seq: 0, flags: 0 }
    }

    pub fn auth2(length: u32) -> Self {
        Self { kind: Kind::Auth2, length, seq: 0, flags: 0 }
    }

    pub fn frame(length: u32, seq: u16) -> Self {
        Self { kind: Kind::Frame, length, seq, flags: FLAGS_FRAME }
    }

    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let magic = self.kind.magic().to_le_bytes();
        let mut out = [0u8; HEADER_LEN];
        out[0x00..0x04].copy_from_slice(&magic);
        out[0x04..0x08].copy_from_slice(&self.length.to_le_bytes());
        out[0x08..0x0A].copy_from_slice(&self.seq.to_le_bytes());
        out[0x0A..0x0C].copy_from_slice(&self.flags.to_le_bytes());
        // 0x0C..0x10 reserved, zero
        out[0x10..0x14].copy_from_slice(&magic);
        out
    }

    /// Parses a header. Returns `None` unless `bytes` is exactly a well-formed
    /// header of a known kind.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let bytes: &[u8; HEADER_LEN] = bytes.try_into().ok()?;
        let u32_at = |i: usize| u32::from_le_bytes(bytes[i..i + 4].try_into().unwrap());
        let u16_at = |i: usize| u16::from_le_bytes(bytes[i..i + 2].try_into().unwrap());

        let magic = u32_at(0x00);
        if magic & 0xFFFF_FF00 != MAGIC_BASE || u32_at(0x10) != magic || u32_at(0x0C) != 0 {
            return None;
        }
        Some(Self {
            kind: Kind::from_u8(magic as u8)?,
            length: u32_at(0x04),
            seq: u16_at(0x08),
            flags: u16_at(0x0A),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_frame_header() {
        let bytes = Header::frame(0x1234, 7).encode();
        assert_eq!(
            bytes,
            [
                0x01, 0x2b, 0xc6, 0xa1, // magic
                0x34, 0x12, 0x00, 0x00, // length
                0x07, 0x00, // seq
                0x10, 0x00, // flags
                0x00, 0x00, 0x00, 0x00, // reserved
                0x01, 0x2b, 0xc6, 0xa1, // magic again
            ]
        );
    }

    #[test]
    fn encodes_auth_headers() {
        assert_eq!(
            Header::auth1(256).encode(),
            [
                0x10, 0x2b, 0xc6, 0xa1, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x00, 0x00, 0x10, 0x2b, 0xc6, 0xa1,
            ]
        );
        assert_eq!(
            Header::auth2(256).encode(),
            [
                0x11, 0x2b, 0xc6, 0xa1, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x00, 0x00, 0x11, 0x2b, 0xc6, 0xa1,
            ]
        );
    }

    #[test]
    fn decode_roundtrips() {
        for header in [Header::auth1(256), Header::auth2(256), Header::frame(48_000, 0xFFFF)] {
            assert_eq!(Header::decode(&header.encode()), Some(header));
        }
    }

    #[test]
    fn decode_rejects_malformed() {
        let good = Header::frame(10, 1).encode();
        assert!(Header::decode(&good[..19]).is_none());

        let mut bad_second_magic = good;
        bad_second_magic[0x10] = 0x10;
        assert!(Header::decode(&bad_second_magic).is_none());

        let mut unknown_kind = good;
        unknown_kind[0] = 0x02;
        unknown_kind[0x10] = 0x02;
        assert!(Header::decode(&unknown_kind).is_none());

        let mut reserved_set = good;
        reserved_set[0x0C] = 1;
        assert!(Header::decode(&reserved_set).is_none());
    }
}
