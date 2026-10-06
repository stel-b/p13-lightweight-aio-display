//! Connection sequence and frame sending, exactly as MSI's driver does it.

use std::borrow::Cow;
use std::fmt;
use std::time::Duration;

use crate::device_info::{DEVICE_INFO_LEN, DeviceInfo, DeviceInfoError};
use crate::handshake::{AUTH1_RESPONSE_LEN, AUTH2_RESPONSE_LEN, Auth, AuthResponses, RSA_BLOCK_LEN, random_challenge};
use crate::header::Header;
use crate::transport::{Transport, TransportError, VendorRequest};
use crate::MAX_PACKET_SIZE;

pub const REQ_DEVICE_INFO: VendorRequest = VendorRequest::new(0, 0, 0);
pub const REQ_SETUP_3: VendorRequest = VendorRequest::new(3, 0, 0);
pub const REQ_SETUP_5: VendorRequest = VendorRequest::new(5, 0x0004, 0);

const CONTROL_TIMEOUT: Duration = Duration::from_millis(1000);
const WRITE_TIMEOUT: Duration = Duration::from_millis(1000);
const DRAIN_TIMEOUT: Duration = Duration::from_millis(200);
const AUTH_READ_TIMEOUT: Duration = Duration::from_millis(3000);
const FRAME_HEADER_TIMEOUT: Duration = Duration::from_millis(2000);
const FRAME_PAYLOAD_TIMEOUT: Duration = Duration::from_millis(3000);
/// Delay MSI's driver leaves between the setup requests and auth step 1.
pub const SETUP_DELAY: Duration = Duration::from_millis(170);
/// Give up draining after this many reads that all returned data.
pub const MAX_DRAIN_READS: usize = 64;

/// Where in the protocol an error happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Drain,
    DeviceInfo,
    Setup,
    Auth1,
    Auth2,
    Final,
    Frame,
}

impl fmt::Display for Step {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Step::Drain => "draining bulk IN",
            Step::DeviceInfo => "reading device info",
            Step::Setup => "setup requests",
            Step::Auth1 => "auth step 1",
            Step::Auth2 => "auth step 2",
            Step::Final => "final handshake block",
            Step::Frame => "sending frame",
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{step}: {source}")]
    Transport {
        step: Step,
        #[source]
        source: TransportError,
    },
    #[error(transparent)]
    DeviceInfo(#[from] DeviceInfoError),
    #[error("device reports a {width}x{height} panel, expected 480x480")]
    UnexpectedPanel { width: u16, height: u16 },
    #[error("{step}: expected a {expected}-byte response, got {actual} bytes")]
    UnexpectedResponse { step: Step, expected: usize, actual: usize },
    #[error("bulk IN still returned data after {MAX_DRAIN_READS} reads")]
    DrainOverflow,
    #[error("frame is not a JPEG")]
    NotJpeg,
    #[error("frame of {0} bytes is too large")]
    FrameTooLarge(usize),
    #[error("auth step 1: the device did not echo our challenge (wrong device key?)")]
    ChallengeMismatch,
    #[error("auth step 2: the device's answer is not a valid signature under the device key")]
    BadSignature,
    #[error("auth step 1: encrypting the challenge failed: {0}")]
    Encrypt(rsa::Error),
}

impl Error {
    /// The underlying transport error, if any (to tell disconnects from
    /// protocol problems).
    pub fn transport_error(&self) -> Option<&TransportError> {
        match self {
            Error::Transport { source, .. } => Some(source),
            _ => None,
        }
    }

    /// True if the failure happened during authentication (as opposed to
    /// device info, setup or frames), e.g. to fall back to another [`Auth`].
    pub fn is_auth_failure(&self) -> bool {
        match self {
            Error::ChallengeMismatch | Error::BadSignature | Error::Encrypt(_) => true,
            Error::Transport { step, .. } | Error::UnexpectedResponse { step, .. } => {
                matches!(step, Step::Auth1 | Step::Auth2 | Step::Final)
            }
            _ => false,
        }
    }
}

trait StepExt<T> {
    fn at(self, step: Step) -> Result<T, Error>;
}

impl<T> StepExt<T> for Result<T, TransportError> {
    fn at(self, step: Step) -> Result<T, Error> {
        self.map_err(|source| Error::Transport { step, source })
    }
}

/// Reads bulk IN until it times out, discarding stale responses the firmware
/// kept from an earlier host session. Returns how many transfers were dropped.
pub fn drain<T: Transport>(transport: &mut T) -> Result<usize, Error> {
    for dropped in 0..MAX_DRAIN_READS {
        match transport.bulk_read(MAX_PACKET_SIZE, DRAIN_TIMEOUT) {
            Ok(_) => continue,
            Err(TransportError::Timeout) => return Ok(dropped),
            Err(e) => return Err(e).at(Step::Drain),
        }
    }
    Err(Error::DrainOverflow)
}

fn read_response<T: Transport>(t: &mut T, step: Step, expected: usize) -> Result<Vec<u8>, Error> {
    let data = t.bulk_read(MAX_PACKET_SIZE, AUTH_READ_TIMEOUT).at(step)?;
    if data.len() != expected {
        return Err(Error::UnexpectedResponse { step, expected, actual: data.len() });
    }
    Ok(data)
}

/// The JPEG as it goes on the wire: one `0x00` is appended when the length is
/// an exact multiple of 512, so the transfer never needs a zero-length packet.
pub fn frame_payload(jpeg: &[u8]) -> Cow<'_, [u8]> {
    if jpeg.len().is_multiple_of(MAX_PACKET_SIZE) {
        let mut padded = Vec::with_capacity(jpeg.len() + 1);
        padded.extend_from_slice(jpeg);
        padded.push(0);
        Cow::Owned(padded)
    } else {
        Cow::Borrowed(jpeg)
    }
}

/// A display that completed the handshake and accepts frames.
pub struct Display<T: Transport> {
    transport: T,
    seq: u16,
    info: DeviceInfo,
    responses: AuthResponses,
}

impl<T: Transport> Display<T> {
    /// Runs the full connection sequence (section 3.2 of the handoff doc).
    pub fn connect(mut transport: T, auth: &Auth) -> Result<Self, Error> {
        let t = &mut transport;
        drain(t)?;

        let raw = t
            .control_in(REQ_DEVICE_INFO, DEVICE_INFO_LEN as u16, CONTROL_TIMEOUT)
            .at(Step::DeviceInfo)?;
        let info = DeviceInfo::parse(&raw)?;
        if !info.is_expected_panel() {
            return Err(Error::UnexpectedPanel { width: info.width, height: info.height });
        }

        t.control_out(REQ_SETUP_3, &[], CONTROL_TIMEOUT).at(Step::Setup)?;
        t.control_out(REQ_SETUP_5, &[], CONTROL_TIMEOUT).at(Step::Setup)?;
        t.sleep(SETUP_DELAY);

        // Auth 1: a challenge only the device can decrypt; it must echo it.
        let mut rng = rand_core::OsRng;
        let challenge = random_challenge(&mut rng);
        let (blob, echo_len): (Cow<[u8]>, usize) = match auth {
            Auth::Computed(key) => {
                (Cow::Owned(key.encrypt(&mut rng, &challenge).map_err(Error::Encrypt)?), challenge.len())
            }
            Auth::Replay(hs) => (Cow::Borrowed(&hs.blob1[..]), AUTH1_RESPONSE_LEN),
        };
        t.bulk_write(&Header::auth1(blob.len() as u32).encode(), WRITE_TIMEOUT).at(Step::Auth1)?;
        t.bulk_write(&blob, WRITE_TIMEOUT).at(Step::Auth1)?;
        // Must be read before anything else is sent, or the device NAKs all OUT.
        let auth1 = read_response(t, Step::Auth1, echo_len)?;
        if matches!(auth, Auth::Computed(_)) && auth1 != challenge {
            return Err(Error::ChallengeMismatch);
        }
        drain(t)?;

        // Auth 2: the device signs its own challenge; we send it back.
        t.bulk_write(&Header::auth2(RSA_BLOCK_LEN as u32).encode(), WRITE_TIMEOUT).at(Step::Auth2)?;
        let auth2 = read_response(t, Step::Auth2, AUTH2_RESPONSE_LEN)?;
        let reply: Cow<[u8]> = match auth {
            Auth::Computed(key) => Cow::Owned(key.recover(&auth2).ok_or(Error::BadSignature)?),
            Auth::Replay(hs) => Cow::Borrowed(&hs.final60[..]),
        };

        // Raw block, no header.
        t.bulk_write(&reply, WRITE_TIMEOUT).at(Step::Final)?;

        Ok(Self { transport, seq: 0, info, responses: AuthResponses { auth1, auth2 } })
    }

    pub fn device_info(&self) -> &DeviceInfo {
        &self.info
    }

    pub fn auth_responses(&self) -> &AuthResponses {
        &self.responses
    }

    /// Sequence number the next frame will carry.
    pub fn next_seq(&self) -> u16 {
        self.seq
    }

    /// Sends one 480×480 baseline JPEG. The device shows it until the next
    /// frame; it sends no acknowledgement.
    pub fn send_jpeg(&mut self, jpeg: &[u8]) -> Result<(), Error> {
        if !jpeg.starts_with(&[0xFF, 0xD8]) {
            return Err(Error::NotJpeg);
        }
        let payload = frame_payload(jpeg);
        let length = u32::try_from(payload.len()).map_err(|_| Error::FrameTooLarge(jpeg.len()))?;

        let t = &mut self.transport;
        t.bulk_write(&Header::frame(length, self.seq).encode(), FRAME_HEADER_TIMEOUT)
            .at(Step::Frame)?;
        t.bulk_write(&payload, FRAME_PAYLOAD_TIMEOUT).at(Step::Frame)?;
        self.seq = self.seq.wrapping_add(1);
        Ok(())
    }

    pub fn transport(&self) -> &T {
        &self.transport
    }

    pub fn transport_mut(&mut self) -> &mut T {
        &mut self.transport
    }

    pub fn into_transport(self) -> T {
        self.transport
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::header::{FLAGS_FRAME, Kind};
    use crate::mock::{Event, MockDevice, mock_device_key, test_handshake};

    const JPEG: &[u8] = &[0xFF, 0xD8, 1, 2, 3, 0xFF, 0xD9];

    fn connect(mock: MockDevice) -> Result<Display<MockDevice>, Error> {
        Display::connect(mock, &Auth::Replay(test_handshake()))
    }

    #[test]
    fn handshake_then_frame_reaches_device() {
        let mut display = connect(MockDevice::new(test_handshake())).unwrap();
        assert!(display.transport().is_ready());
        assert_eq!(display.device_info().width, 480);

        display.send_jpeg(JPEG).unwrap();
        let frames = &display.transport().frames;
        assert_eq!(frames.len(), 1);
        let (header, payload) = &frames[0];
        assert_eq!(header.kind, Kind::Frame);
        assert_eq!(header.seq, 0);
        assert_eq!(header.flags, FLAGS_FRAME);
        assert_eq!(header.length as usize, JPEG.len());
        assert_eq!(payload, JPEG);
    }

    #[test]
    fn follows_msi_sequence() {
        let hs = test_handshake();
        let display = connect(MockDevice::new(hs.clone())).unwrap();
        let events = &display.transport().events;
        let expected = vec![
            Event::BulkRead(None), // drain finds nothing
            Event::ControlIn(REQ_DEVICE_INFO, 160),
            Event::ControlOut(REQ_SETUP_3, vec![]),
            Event::ControlOut(REQ_SETUP_5, vec![]),
            Event::Sleep(SETUP_DELAY),
            Event::BulkWrite(Header::auth1(256).encode().to_vec()),
            Event::BulkWrite(hs.blob1.to_vec()),
            Event::BulkRead(Some(202)),
            Event::BulkRead(None), // defensive drain
            Event::BulkWrite(Header::auth2(256).encode().to_vec()),
            Event::BulkRead(Some(256)),
            Event::BulkWrite(hs.final60.to_vec()),
        ];
        assert_eq!(events, &expected);
    }

    #[test]
    fn keeps_device_responses() {
        let mock = MockDevice::new(test_handshake());
        let expected = mock.responses.clone();
        let display = connect(mock).unwrap();
        assert_eq!(display.auth_responses(), &expected);
    }

    #[test]
    fn drains_stale_responses_from_previous_session() {
        let mut mock = MockDevice::new(test_handshake());
        mock.queue_stale_in(3);
        let display = connect(mock).unwrap();
        assert!(display.transport().is_ready());
    }

    #[test]
    fn stale_responses_block_handshake_without_drain() {
        // The rule the drain exists for: OUT is NAKed while IN data is unread.
        let mut mock = MockDevice::new(test_handshake());
        mock.queue_stale_in(1);
        let err = mock.bulk_write(&Header::auth1(256).encode(), WRITE_TIMEOUT).unwrap_err();
        assert!(matches!(err, TransportError::Timeout));
    }

    #[test]
    fn endless_in_data_gives_up() {
        let mut mock = MockDevice::new(test_handshake());
        mock.endless_in = true;
        assert!(matches!(connect(mock), Err(Error::DrainOverflow)));
    }

    #[test]
    fn rejected_blob_times_out_at_auth1() {
        let mut wrong = test_handshake();
        wrong.blob1[0] ^= 0xFF;
        let err = Display::connect(MockDevice::new(test_handshake()), &Auth::Replay(wrong)).err().unwrap();
        assert!(matches!(
            err,
            Error::Transport { step: Step::Auth1, source: TransportError::Timeout }
        ));
    }

    #[test]
    fn rejects_unexpected_response_length() {
        let mut mock = MockDevice::new(test_handshake());
        mock.responses.auth2.truncate(200);
        let err = connect(mock).err().unwrap();
        assert!(matches!(
            err,
            Error::UnexpectedResponse { step: Step::Auth2, expected: 256, actual: 200 }
        ));
    }

    #[test]
    fn rejects_other_panel_before_any_bulk_write() {
        let mut mock = MockDevice::new(test_handshake());
        mock.info[10] = 0x40; // width 320
        mock.info[11] = 0x01;
        let err = connect(mock).err().unwrap();
        assert!(matches!(err, Error::UnexpectedPanel { width: 320, height: 480 }));
    }

    #[test]
    fn frame_without_handshake_crashes_mock() {
        // Documents the firmware behavior Display::connect prevents.
        let mut mock = MockDevice::new(test_handshake());
        let err = mock.bulk_write(&Header::frame(7, 0).encode(), WRITE_TIMEOUT).unwrap_err();
        assert!(matches!(err, TransportError::Disconnected));
        assert!(mock.crashed);
    }

    #[test]
    fn sequence_increments_and_wraps() {
        let mut display = connect(MockDevice::new(test_handshake())).unwrap();
        display.send_jpeg(JPEG).unwrap();
        display.send_jpeg(JPEG).unwrap();
        display.seq = 0xFFFF;
        display.send_jpeg(JPEG).unwrap();
        display.send_jpeg(JPEG).unwrap();
        let seqs: Vec<u16> = display.transport().frames.iter().map(|(h, _)| h.seq).collect();
        assert_eq!(seqs, [0, 1, 0xFFFF, 0]);
    }

    #[test]
    fn pads_payload_that_fills_whole_packets() {
        let mut jpeg = vec![0u8; 1024];
        jpeg[..2].copy_from_slice(&[0xFF, 0xD8]);
        let mut display = connect(MockDevice::new(test_handshake())).unwrap();
        display.send_jpeg(&jpeg).unwrap();
        let (header, payload) = &display.transport().frames[0];
        assert_eq!(header.length, 1025);
        assert_eq!(payload.len(), 1025);
        assert_eq!(payload[1024], 0);
        assert_eq!(&payload[..1024], &jpeg[..]);
    }

    #[test]
    fn frame_payload_borrows_when_no_padding_needed() {
        assert!(matches!(frame_payload(&[0; 1000]), Cow::Borrowed(_)));
        assert_eq!(frame_payload(&[0; 512]).len(), 513);
    }

    #[test]
    fn rejects_non_jpeg_without_sending() {
        let mut display = connect(MockDevice::new(test_handshake())).unwrap();
        let before = display.transport().events.len();
        assert!(matches!(display.send_jpeg(b"GIF89a"), Err(Error::NotJpeg)));
        assert_eq!(display.transport().events.len(), before);
    }

    fn rsa_connect(mock: MockDevice, key: crate::DeviceKey) -> Result<Display<MockDevice>, Error> {
        Display::connect(mock, &Auth::Computed(key))
    }

    #[test]
    fn computed_handshake_then_frame() {
        let mut display = rsa_connect(MockDevice::new_rsa(), mock_device_key()).unwrap();
        assert!(display.transport().is_ready());
        display.send_jpeg(JPEG).unwrap();
        assert_eq!(display.transport().frames.len(), 1);
        // The device echoed our challenge: alphanumeric, MSI's length range.
        let echo = &display.auth_responses().auth1;
        assert!(crate::handshake::CHALLENGE_LEN.contains(&echo.len()));
        assert!(echo.iter().all(u8::is_ascii_alphanumeric));
    }

    #[test]
    fn computed_handshake_uses_a_fresh_challenge() {
        let blob = |d: &Display<MockDevice>| match &d.transport().events[6] {
            Event::BulkWrite(b) => b.clone(),
            e => panic!("unexpected event {e:?}"),
        };
        let a = rsa_connect(MockDevice::new_rsa(), mock_device_key()).unwrap();
        let b = rsa_connect(MockDevice::new_rsa(), mock_device_key()).unwrap();
        assert_eq!(blob(&a).len(), 256);
        assert_ne!(blob(&a), blob(&b));
        assert_ne!(a.auth_responses().auth1, b.auth_responses().auth1);
    }

    #[test]
    fn wrong_device_key_gets_no_answer() {
        let other = rsa::RsaPrivateKey::new(&mut rand_core::OsRng, 2048).unwrap();
        let err = rsa_connect(MockDevice::new_rsa(), crate::DeviceKey::new(other.to_public_key()).unwrap())
            .err()
            .unwrap();
        assert!(matches!(err, Error::Transport { step: Step::Auth1, source: TransportError::Timeout }));
        assert!(err.is_auth_failure());
    }

    #[test]
    fn forged_signature_is_rejected() {
        let mut mock = MockDevice::new_rsa();
        mock.corrupt_signature = true;
        let err = rsa_connect(mock, mock_device_key()).err().unwrap();
        assert!(matches!(err, Error::BadSignature), "{err}");
        assert!(err.is_auth_failure());
    }

    #[test]
    fn non_auth_errors_are_not_auth_failures() {
        let mut mock = MockDevice::new_rsa();
        mock.endless_in = true;
        assert!(!rsa_connect(mock, mock_device_key()).err().unwrap().is_auth_failure());
    }

    #[test]
    fn disconnect_mid_frame_is_reported() {
        let mut display = connect(MockDevice::new(test_handshake())).unwrap();
        display.transport.unplugged = true;
        let err = display.send_jpeg(JPEG).unwrap_err();
        assert!(matches!(err.transport_error(), Some(TransportError::Disconnected)));
    }
}
