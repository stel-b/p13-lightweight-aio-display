//! A simulated P13 firmware for tests. It models the behaviors seen on real
//! hardware rather than replaying a fixed script:
//!
//! - bulk OUT is NAKed (times out) while a bulk IN response is unread;
//! - stale IN data from an earlier session must be drained first;
//! - auth step 1 is only answered after both setup requests and a challenge
//!   it can decrypt (or, in replay mode, the captured blob);
//! - a frame before the handshake completes crashes the firmware.

use std::collections::VecDeque;
use std::time::Duration;

use crate::device_info;
use crate::display::{REQ_DEVICE_INFO, REQ_SETUP_3, REQ_SETUP_5};
use rsa::pkcs8::DecodePrivateKey;
use rsa::traits::{PrivateKeyParts, PublicKeyParts};
use rsa::{BigUint, Pkcs1v15Encrypt, RsaPrivateKey};

use crate::handshake::{
    AUTH1_RESPONSE_LEN, AUTH2_RESPONSE_LEN, AuthResponses, BLOB1_LEN, DeviceKey, FINAL_LEN, HandshakeData,
    RSA_BLOCK_LEN,
};
use crate::header::{Header, Kind};
use crate::transport::{Transport, TransportError, VendorRequest};

/// Placeholder handshake data (the real blob is never in the repo).
pub fn test_handshake() -> HandshakeData {
    HandshakeData {
        blob1: std::array::from_fn(|i| i as u8),
        final60: std::array::from_fn(|i| 0xF0 ^ i as u8),
    }
}

/// A throwaway RSA-2048 key standing in for the real device's private key.
fn mock_private_key() -> RsaPrivateKey {
    RsaPrivateKey::from_pkcs8_pem(include_str!("mock_device_key.pem")).expect("bundled test key")
}

/// The public half of the mock device's key, for [`crate::Auth::Computed`].
pub fn mock_device_key() -> DeviceKey {
    DeviceKey::new(mock_private_key().to_public_key()).expect("2048-bit test key")
}

/// [`mock_device_key`] as a PEM `PUBLIC KEY`, like `device_key.pem`.
pub fn mock_device_key_pem() -> String {
    use rsa::pkcs8::{EncodePublicKey, LineEnding};
    mock_private_key().to_public_key().to_public_key_pem(LineEnding::LF).expect("encodable key")
}

/// How the simulated firmware authenticates the host.
enum Credentials {
    Replay(HandshakeData),
    Rsa { key: RsaPrivateKey, challenge: Vec<u8> },
}

/// Everything the host did, in order. Bulk reads record the length returned,
/// or `None` for a timeout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    ControlIn(VendorRequest, u16),
    ControlOut(VendorRequest, Vec<u8>),
    BulkWrite(Vec<u8>),
    BulkRead(Option<usize>),
    Sleep(Duration),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Idle,
    AwaitBlob1,
    AwaitFinal,
    Ready,
    AwaitFramePayload(Header),
}

pub struct MockDevice {
    credentials: Credentials,
    /// RSA mode: send a corrupted signature at auth step 2.
    pub corrupt_signature: bool,
    /// Returned by control request 0.
    pub info: Vec<u8>,
    /// What the firmware answers to the auth steps.
    pub responses: AuthResponses,
    /// Frames received after the handshake: header and payload.
    pub frames: Vec<(Header, Vec<u8>)>,
    pub events: Vec<Event>,
    /// Set when the firmware got a frame it couldn't handle and rebooted.
    pub crashed: bool,
    /// Simulates the cable being pulled: every call fails with `Disconnected`.
    pub unplugged: bool,
    /// Bulk IN never runs dry.
    pub endless_in: bool,
    pending_in: VecDeque<Vec<u8>>,
    setup: [bool; 2],
    auth1_done: bool,
    state: State,
}

impl MockDevice {
    /// A device that accepts the replay of `expected`.
    pub fn new(expected: HandshakeData) -> Self {
        Self::with_credentials(Credentials::Replay(expected))
    }

    /// A device that runs the real RSA handshake with [`mock_device_key`].
    pub fn new_rsa() -> Self {
        let challenge = b"0Fmock0device0challenge0EUW6RHqP2bTz0ZY4QlX8cVnJ1kdS7aG3".to_vec();
        Self::with_credentials(Credentials::Rsa { key: mock_private_key(), challenge })
    }

    fn with_credentials(credentials: Credentials) -> Self {
        Self {
            credentials,
            corrupt_signature: false,
            info: device_info::sample_bytes(),
            responses: AuthResponses {
                auth1: vec![b'Q'; AUTH1_RESPONSE_LEN],
                auth2: vec![0x5A; AUTH2_RESPONSE_LEN],
            },
            frames: Vec::new(),
            events: Vec::new(),
            crashed: false,
            unplugged: false,
            endless_in: false,
            pending_in: VecDeque::new(),
            setup: [false; 2],
            auth1_done: false,
            state: State::Idle,
        }
    }

    /// Leaves unread responses in bulk IN, as after an interrupted session.
    pub fn queue_stale_in(&mut self, count: usize) {
        for _ in 0..count {
            self.pending_in.push_back(vec![0xEE; 202]);
        }
    }

    pub fn is_ready(&self) -> bool {
        self.state == State::Ready
    }

    fn check_alive(&self) -> Result<(), TransportError> {
        if self.crashed || self.unplugged { Err(TransportError::Disconnected) } else { Ok(()) }
    }

    fn crash(&mut self) -> TransportError {
        self.crashed = true;
        TransportError::Disconnected
    }

    fn handle_out(&mut self, data: &[u8]) -> Result<(), TransportError> {
        match self.state {
            State::AwaitBlob1 => {
                let answer = match &self.credentials {
                    Credentials::Replay(hs) => {
                        (data.len() == BLOB1_LEN && data == hs.blob1).then(|| self.responses.auth1.clone())
                    }
                    // A challenge it can't decrypt goes unanswered.
                    Credentials::Rsa { key, .. } => key.decrypt(Pkcs1v15Encrypt, data).ok(),
                };
                if let Some(answer) = answer {
                    self.pending_in.push_back(answer);
                    self.auth1_done = true;
                }
                self.state = State::Idle;
            }
            State::AwaitFinal => {
                let accepted = match &self.credentials {
                    Credentials::Replay(hs) => data.len() == FINAL_LEN && data == hs.final60,
                    Credentials::Rsa { challenge, .. } => data == challenge.as_slice(),
                };
                self.state = if accepted { State::Ready } else { State::Idle };
            }
            State::AwaitFramePayload(header) => {
                if data.len() != header.length as usize {
                    return Err(self.crash());
                }
                self.frames.push((header, data.to_vec()));
                self.state = State::Ready;
            }
            State::Idle | State::Ready => {
                let Some(header) = Header::decode(data) else {
                    return Err(self.crash());
                };
                match header.kind {
                    Kind::Auth1 if self.setup == [true, true] && header.length == 256 => {
                        self.state = State::AwaitBlob1;
                    }
                    Kind::Auth2 if self.auth1_done => {
                        let answer = match &self.credentials {
                            Credentials::Replay(_) => self.responses.auth2.clone(),
                            Credentials::Rsa { key, challenge } => {
                                let mut sig = sign(key, challenge);
                                if self.corrupt_signature {
                                    sig[100] ^= 0x55;
                                }
                                sig
                            }
                        };
                        self.pending_in.push_back(answer);
                        self.state = State::AwaitFinal;
                    }
                    Kind::Frame if self.state == State::Ready => {
                        self.state = State::AwaitFramePayload(header);
                    }
                    Kind::Frame => return Err(self.crash()),
                    _ => {} // ignored: the host then times out waiting
                }
            }
        }
        Ok(())
    }
}

impl Transport for MockDevice {
    fn control_in(
        &mut self,
        req: VendorRequest,
        length: u16,
        _timeout: Duration,
    ) -> Result<Vec<u8>, TransportError> {
        self.events.push(Event::ControlIn(req, length));
        self.check_alive()?;
        if req != REQ_DEVICE_INFO {
            return Err(TransportError::Stall);
        }
        Ok(self.info.iter().copied().take(length as usize).collect())
    }

    fn control_out(
        &mut self,
        req: VendorRequest,
        data: &[u8],
        _timeout: Duration,
    ) -> Result<(), TransportError> {
        self.events.push(Event::ControlOut(req, data.to_vec()));
        self.check_alive()?;
        match req {
            r if r == REQ_SETUP_3 => self.setup[0] = true,
            r if r == REQ_SETUP_5 => self.setup[1] = true,
            _ => return Err(TransportError::Stall),
        }
        Ok(())
    }

    fn bulk_write(&mut self, data: &[u8], _timeout: Duration) -> Result<(), TransportError> {
        self.events.push(Event::BulkWrite(data.to_vec()));
        self.check_alive()?;
        if !self.pending_in.is_empty() || self.endless_in {
            return Err(TransportError::Timeout);
        }
        self.handle_out(data)
    }

    fn bulk_read(&mut self, max_len: usize, _timeout: Duration) -> Result<Vec<u8>, TransportError> {
        self.check_alive()?;
        let data = if self.endless_in { Some(vec![0xEE; 16]) } else { self.pending_in.pop_front() };
        self.events.push(Event::BulkRead(data.as_ref().map(Vec::len)));
        match data {
            Some(d) => {
                assert!(d.len() <= max_len, "read buffer too small for response");
                Ok(d)
            }
            None => Err(TransportError::Timeout),
        }
    }

    fn sleep(&mut self, duration: Duration) {
        self.events.push(Event::Sleep(duration));
    }
}

/// PKCS#1 v1.5 block type 1 over `payload`, raised to the private exponent:
/// what the firmware sends at auth step 2.
fn sign(key: &RsaPrivateKey, payload: &[u8]) -> Vec<u8> {
    let mut block = vec![0xFFu8; RSA_BLOCK_LEN];
    block[0] = 0x00;
    block[1] = 0x01;
    let sep = RSA_BLOCK_LEN - payload.len() - 1;
    block[sep] = 0x00;
    block[sep + 1..].copy_from_slice(payload);
    let s = BigUint::from_bytes_be(&block).modpow(key.d(), key.n()).to_bytes_be();
    let mut out = vec![0u8; RSA_BLOCK_LEN - s.len()];
    out.extend(s);
    out
}

/// A simulated HID control interface: parses requests, keeps the settings
/// they change, and answers like the firmware (`1 200`, `AckNumber=seq+1`).
#[derive(Debug)]
pub struct MockHid {
    /// Requests received, as `"<cmd>"` or `"<cmd> <json body>"`.
    pub requests: Vec<String>,
    pub brightness: u8,
    pub degree: u16,
    pub extended_display: bool,
    /// Never answer (the client should retry once, then give up).
    pub silent: bool,
    /// Every call fails with `Disconnected`.
    pub unplugged: bool,
    pending: VecDeque<Vec<u8>>,
}

impl Default for MockHid {
    fn default() -> Self {
        Self {
            requests: Vec::new(),
            brightness: 100,
            degree: 180,
            extended_display: true,
            silent: false,
            unplugged: false,
            pending: VecDeque::new(),
        }
    }
}

impl MockHid {
    fn answer(&mut self, seq: u16, body: Option<String>) {
        let mut msg = format!("1 200\r\nAckNumber={}\r\n", seq + 1);
        match body {
            Some(b) => msg.push_str(&format!("ContentType=json\r\nContentLength={}\r\n\r\n{b}", b.len())),
            None => msg.push_str("\r\n"),
        }
        let frame = crate::hid::encode_frame(msg.as_bytes()).expect("small answer");
        self.pending.push_back(crate::hid::to_report(&frame).to_vec());
    }

    fn number(body: &str) -> Option<u16> {
        let digits: String = body.chars().filter(char::is_ascii_digit).collect();
        digits.parse().ok()
    }
}

impl crate::hid::HidTransport for MockHid {
    fn write_report(&mut self, report: &[u8; crate::hid::REPORT_LEN]) -> Result<(), TransportError> {
        if self.unplugged {
            return Err(TransportError::Disconnected);
        }
        let message = crate::hid::decode_frame(report).map_err(|e| TransportError::Other(e.to_string()))?;
        let text = String::from_utf8_lossy(&message).into_owned();
        let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
        let mut lines = head.split("\r\n");
        let cmd = lines.next().unwrap_or_default().split(' ').nth(1).unwrap_or_default().to_owned();
        let seq: u16 = lines.find_map(|l| l.strip_prefix("SeqNumber=")?.parse().ok()).unwrap_or(0);
        self.requests.push(if body.is_empty() { cmd.clone() } else { format!("{cmd} {body}") });

        let reply = match cmd.as_str() {
            "conn" => Some(format!(
                "{{\"OS\":\"RTOS\",\"Manufacturer\":\"MSI\",\"model\":\"MPG CORELIQUID P13 Series\",\
                 \"version\":{{\"app\":\"V1.0.11\",\"firmware\":\"P13_20251204v01\",\"sdk\":\"V1.2.0\",\"hardware\":\"V2.0\"}},\
                 \"brightness\":{},\"degree\":{},\"sn\":\"TEST\",\"bootFinish\":1,\"realtimeDisplay\":0,\"extendedDisplay\":{}}}",
                self.brightness,
                self.degree,
                u8::from(self.extended_display)
            )),
            "brightness" => {
                self.brightness = Self::number(body).unwrap_or(0) as u8;
                None
            }
            "rotate" => {
                self.degree = Self::number(body).unwrap_or(0);
                None
            }
            "extendedDisplay" => {
                self.extended_display = body.contains("true");
                None
            }
            _ => return Ok(()), // unknown commands are ignored, like garbage
        };
        if !self.silent {
            self.answer(seq, reply);
        }
        Ok(())
    }

    fn read_report(&mut self, _timeout: Duration) -> Result<Vec<u8>, TransportError> {
        if self.unplugged {
            return Err(TransportError::Disconnected);
        }
        self.pending.pop_front().ok_or(TransportError::Timeout)
    }

    fn flush_input(&mut self) -> Result<(), TransportError> {
        self.pending.clear();
        Ok(())
    }
}
