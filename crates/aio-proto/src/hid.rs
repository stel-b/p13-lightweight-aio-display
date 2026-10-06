//! Control channel on the HID interface (interface 1). See `docs/hid-protocol.md`.
//!
//! Requests are HTTP-like text (`POST <cmd> 1`, headers, JSON body) wrapped in
//! a small frame and sent as one 1024-byte report; the device answers with
//! `1 200` and `AckNumber=<seq+1>` the same way.
//!
//! Only [`Command`]'s variants can be built. MSI's client also knows
//! `transport`, `reboot` and `upgrade` (firmware); those are deliberately not
//! expressible here.

use std::time::{Duration, Instant};

use crate::transport::TransportError;

/// Size of one HID report (without the Windows report-ID byte).
pub const REPORT_LEN: usize = 1024;
const FLAG: u8 = 0x5A;
const ESCAPE: u8 = 0x5B;
/// MSI waits up to 3 s for an answer.
pub const RESPONSE_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug, thiserror::Error)]
pub enum HidError {
    #[error("HID transfer failed: {0}")]
    Transport(#[from] TransportError),
    #[error("malformed HID frame: {0}")]
    Frame(&'static str),
    #[error("malformed HID response: {0}")]
    Response(String),
    #[error("device answered {status} to {command}")]
    Status { command: &'static str, status: u16 },
    #[error("no answer to {0}")]
    NoAnswer(&'static str),
    #[error("{0}")]
    Invalid(String),
}

/// Screen rotations MSI's software offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rotation {
    Deg0,
    Deg90,
    Deg180,
    Deg270,
}

impl Rotation {
    pub fn from_degrees(degrees: u16) -> Option<Self> {
        match degrees {
            0 => Some(Rotation::Deg0),
            90 => Some(Rotation::Deg90),
            180 => Some(Rotation::Deg180),
            270 => Some(Rotation::Deg270),
            _ => None,
        }
    }

    pub fn degrees(self) -> u16 {
        match self {
            Rotation::Deg0 => 0,
            Rotation::Deg90 => 90,
            Rotation::Deg180 => 180,
            Rotation::Deg270 => 270,
        }
    }
}

/// The only requests this crate can send.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// Device info (firmware, brightness, rotation, mode flags). Read-only.
    Conn,
    /// Backlight, 0..=100. 0 is MSI's "LCD off". The device remembers it.
    Brightness(u8),
    Rotate(Rotation),
    /// MSI turns this off at Windows shutdown and on at startup and resume.
    ExtendedDisplay(bool),
}

impl Command {
    pub fn name(&self) -> &'static str {
        match self {
            Command::Conn => "conn",
            Command::Brightness(_) => "brightness",
            Command::Rotate(_) => "rotate",
            Command::ExtendedDisplay(_) => "extendedDisplay",
        }
    }

    /// Sequence numbers MSI uses; the answer carries `seq + 1`.
    pub fn seq(&self) -> u16 {
        match self {
            Command::ExtendedDisplay(_) => 110,
            _ => 100,
        }
    }

    fn body(&self) -> Option<String> {
        match self {
            Command::Conn => None,
            Command::Brightness(v) => Some(format!("{{\"value\":{v}}}")),
            Command::Rotate(r) => Some(format!("{{\"degree\":{}}}", r.degrees())),
            Command::ExtendedDisplay(on) => Some(format!("{{\"enable\":{on}}}")),
        }
    }

    /// The request text, byte for byte as MSI's client builds it.
    pub fn message(&self) -> String {
        let head = format!("POST {} 1\r\nSeqNumber={}\r\nContentType=json", self.name(), self.seq());
        match self.body() {
            Some(body) => format!("{head}\r\nContentLength={}\r\n\r\n{body}", body.len()),
            None => head,
        }
    }

    fn validate(&self) -> Result<(), HidError> {
        match self {
            Command::Brightness(v) if *v > 100 => Err(HidError::Invalid(format!("brightness {v} > 100"))),
            _ => Ok(()),
        }
    }
}

/// Wraps `message` in a frame: `5A | len (u16 BE) | message | checksum | 5A`,
/// escaping `5A`/`5B` between the delimiters.
pub fn encode_frame(message: &[u8]) -> Result<Vec<u8>, HidError> {
    let total = message.len() + 5; // flag + length + checksum + flag
    let len = u16::try_from(total).map_err(|_| HidError::Frame("message too long"))?;
    let mut inner = Vec::with_capacity(total);
    inner.extend_from_slice(&len.to_be_bytes());
    inner.extend_from_slice(message);
    let checksum = inner.iter().fold(0u8, |a, &b| a.wrapping_add(b));
    inner.push(checksum);

    let mut frame = Vec::with_capacity(total + 8);
    frame.push(FLAG);
    for &b in &inner {
        match b {
            FLAG => frame.extend_from_slice(&[ESCAPE, 0x01]),
            ESCAPE => frame.extend_from_slice(&[ESCAPE, 0x02]),
            _ => frame.push(b),
        }
    }
    frame.push(FLAG);
    if frame.len() > REPORT_LEN {
        return Err(HidError::Frame("frame does not fit in one report"));
    }
    Ok(frame)
}

/// Extracts the message from a received report (frame plus zero padding).
pub fn decode_frame(report: &[u8]) -> Result<Vec<u8>, HidError> {
    if report.first() != Some(&FLAG) {
        return Err(HidError::Frame("no start flag"));
    }
    // Escaping guarantees the next raw flag is the end.
    let end = 1 + report[1..].iter().position(|&b| b == FLAG).ok_or(HidError::Frame("no end flag"))?;
    let mut inner = Vec::with_capacity(end);
    let mut bytes = report[1..end].iter();
    while let Some(&b) = bytes.next() {
        inner.push(match b {
            ESCAPE => match bytes.next() {
                Some(0x01) => FLAG,
                Some(0x02) => ESCAPE,
                _ => return Err(HidError::Frame("bad escape")),
            },
            _ => b,
        });
    }
    if inner.len() < 3 {
        return Err(HidError::Frame("too short"));
    }
    let len = usize::from(u16::from_be_bytes([inner[0], inner[1]]));
    if len != inner.len() + 2 {
        return Err(HidError::Frame("length mismatch"));
    }
    let (body, checksum) = inner.split_at(inner.len() - 1);
    if body.iter().fold(0u8, |a, &b| a.wrapping_add(b)) != checksum[0] {
        return Err(HidError::Frame("checksum mismatch"));
    }
    Ok(body[2..].to_vec())
}

/// A frame zero-padded to one report.
pub fn to_report(frame: &[u8]) -> [u8; REPORT_LEN] {
    let mut report = [0u8; REPORT_LEN];
    report[..frame.len()].copy_from_slice(frame);
    report
}

/// A parsed device answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    pub ack: u16,
    /// JSON text, empty when the answer has no body.
    pub body: String,
}

pub fn parse_response(message: &[u8]) -> Result<Response, HidError> {
    let text = String::from_utf8_lossy(message);
    let (head, rest) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|l| l.split_once(' '))
        .and_then(|(_, code)| code.trim().parse().ok())
        .ok_or_else(|| HidError::Response(format!("no status line in {head:?}")))?;
    let mut ack = None;
    let mut content_len = None;
    for line in lines {
        if let Some(v) = line.strip_prefix("AckNumber=") {
            ack = v.trim().parse().ok();
        } else if let Some(v) = line.strip_prefix("ContentLength=") {
            content_len = v.trim().parse::<usize>().ok();
        }
    }
    let ack = ack.ok_or_else(|| HidError::Response("no AckNumber".into()))?;
    let body = match content_len {
        Some(n) => rest.get(..n).unwrap_or(rest),
        None => rest,
    };
    Ok(Response { status, ack, body: body.to_owned() })
}

/// Raw access to the HID interface.
pub trait HidTransport {
    /// Sends one output report.
    fn write_report(&mut self, report: &[u8; REPORT_LEN]) -> Result<(), TransportError>;
    /// Reads one input report; [`TransportError::Timeout`] if none arrives.
    fn read_report(&mut self, timeout: Duration) -> Result<Vec<u8>, TransportError>;
    /// Drops input reports that arrived earlier (e.g. duplicate answers).
    fn flush_input(&mut self) -> Result<(), TransportError> {
        Ok(())
    }
}

pub struct HidClient<T: HidTransport> {
    transport: T,
}

impl<T: HidTransport> HidClient<T> {
    pub fn new(transport: T) -> Self {
        Self { transport }
    }

    pub fn transport(&self) -> &T {
        &self.transport
    }

    pub fn transport_mut(&mut self) -> &mut T {
        &mut self.transport
    }

    /// Sends `command` and waits for the answer with the matching
    /// `AckNumber`, sending once more if the first attempt goes unanswered
    /// (MSI always sends twice).
    pub fn request(&mut self, command: Command) -> Result<Response, HidError> {
        command.validate()?;
        let report = to_report(&encode_frame(command.message().as_bytes())?);
        let want_ack = command.seq() + 1;
        for _attempt in 0..2 {
            self.transport.flush_input()?;
            self.transport.write_report(&report)?;
            let deadline = Instant::now() + RESPONSE_TIMEOUT;
            loop {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    break;
                }
                let raw = match self.transport.read_report(left) {
                    Ok(raw) => raw,
                    Err(TransportError::Timeout) => break,
                    Err(e) => return Err(e.into()),
                };
                // Skip anything that isn't the answer to this request.
                let Ok(response) = decode_frame(&raw).and_then(|m| parse_response(&m)) else { continue };
                if response.ack != want_ack {
                    continue;
                }
                if response.status != 200 {
                    return Err(HidError::Status { command: command.name(), status: response.status });
                }
                return Ok(response);
            }
        }
        Err(HidError::NoAnswer(command.name()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_match_msi() {
        assert_eq!(
            Command::Brightness(0).message(),
            "POST brightness 1\r\nSeqNumber=100\r\nContentType=json\r\nContentLength=11\r\n\r\n{\"value\":0}"
        );
        assert_eq!(Command::Conn.message(), "POST conn 1\r\nSeqNumber=100\r\nContentType=json");
        assert_eq!(
            Command::ExtendedDisplay(true).message(),
            "POST extendedDisplay 1\r\nSeqNumber=110\r\nContentType=json\r\nContentLength=15\r\n\r\n{\"enable\":true}"
        );
        assert_eq!(
            Command::Rotate(Rotation::Deg90).message(),
            "POST rotate 1\r\nSeqNumber=100\r\nContentType=json\r\nContentLength=13\r\n\r\n{\"degree\":90}"
        );
    }

    /// Lengths and checksums of the brightness reports in the USB captures.
    #[test]
    fn frames_match_captures() {
        for (value, wire_len, checksum) in [(0u8, 88, 0x2E), (50, 89, 0x65), (100, 91, 0x93)] {
            let frame = encode_frame(Command::Brightness(value).message().as_bytes()).unwrap();
            assert_eq!(frame.len(), wire_len, "value {value}");
            assert_eq!(frame[frame.len() - 2], checksum, "value {value}");
            assert_eq!((frame[0], frame[frame.len() - 1]), (0x5A, 0x5A));
        }
        // value 100: the length 90 = 0x5A must be escaped.
        let frame = encode_frame(Command::Brightness(100).message().as_bytes()).unwrap();
        assert_eq!(&frame[..4], &[0x5A, 0x00, 0x5B, 0x01]);
    }

    #[test]
    fn decodes_captured_ack() {
        let mut report = vec![0x5A, 0x00, 0x1D];
        report.extend_from_slice(b"1 200\r\nAckNumber=101\r\n\r\n");
        report.extend_from_slice(&[0x8C, 0x5A]);
        report.resize(REPORT_LEN, 0);
        let response = parse_response(&decode_frame(&report).unwrap()).unwrap();
        assert_eq!(response, Response { status: 200, ack: 101, body: String::new() });
    }

    #[test]
    fn escapes_both_special_bytes() {
        let message = [b'a', 0x5A, b'b', 0x5B, b'c'];
        let frame = encode_frame(&message).unwrap();
        assert!(!frame[1..frame.len() - 1].contains(&0x5A));
        assert_eq!(decode_frame(&to_report(&frame)).unwrap(), message);
    }

    #[test]
    fn rejects_corrupt_frames() {
        let mut frame = encode_frame(b"1 200\r\nAckNumber=101\r\n\r\n").unwrap();
        let n = frame.len();
        frame[5] ^= 1;
        assert!(matches!(decode_frame(&frame), Err(HidError::Frame("checksum mismatch"))));
        assert!(decode_frame(&[0x00, 0x5A]).is_err());
        assert!(decode_frame(&frame[..n - 1]).is_err());
    }

    #[test]
    fn parses_body_by_content_length() {
        let msg = b"1 200\r\nAckNumber=101\r\nContentType=json\r\nContentLength=7\r\n\r\n{\"a\":1}junk";
        let response = parse_response(msg).unwrap();
        assert_eq!(response.body, "{\"a\":1}");
    }

    use crate::mock::MockHid;

    #[test]
    fn client_round_trips() {
        let mut client = HidClient::new(MockHid::default());
        let info = client.request(Command::Conn).unwrap();
        assert!(info.body.contains("\"firmware\":\"P13_20251204v01\""), "{}", info.body);
        client.request(Command::Brightness(0)).unwrap();
        client.request(Command::ExtendedDisplay(false)).unwrap();
        let hid = client.transport();
        assert_eq!(hid.brightness, 0);
        assert!(!hid.extended_display);
        assert_eq!(hid.requests, ["conn", "brightness {\"value\":0}", "extendedDisplay {\"enable\":false}"]);
    }

    #[test]
    fn unanswered_request_is_sent_twice_then_fails() {
        let mut hid = MockHid::default();
        hid.silent = true;
        let mut client = HidClient::new(hid);
        let err = client.request(Command::Brightness(50)).unwrap_err();
        assert!(matches!(err, HidError::NoAnswer("brightness")));
        assert_eq!(client.transport().requests.len(), 2);
    }

    #[test]
    fn out_of_range_brightness_is_never_sent() {
        let mut client = HidClient::new(MockHid::default());
        assert!(client.request(Command::Brightness(150)).is_err());
        assert!(client.transport().requests.is_empty());
    }

    #[test]
    fn brightness_is_range_checked() {
        assert!(Command::Brightness(101).validate().is_err());
        assert_eq!(Rotation::from_degrees(45), None);
        assert_eq!(Rotation::from_degrees(270).map(Rotation::degrees), Some(270));
    }
}
