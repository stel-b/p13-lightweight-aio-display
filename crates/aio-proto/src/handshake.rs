//! Handshake credentials. See `docs/handshake.md`.
//!
//! The handshake is a mutual RSA-2048 challenge-response, and the host only
//! needs the device's public key ([`DeviceKey`]): it encrypts a random
//! challenge for the device, and recovers the device's signed challenge.
//! The older replay of a captured session ([`HandshakeData`]) is kept as a
//! fallback. Neither the key nor the replay data is committed to the repo.

use std::fmt;

use rand_core::{CryptoRngCore, RngCore};
use rsa::pkcs8::DecodePublicKey;
use rsa::traits::PublicKeyParts;
use rsa::{BigUint, Pkcs1v15Encrypt, RsaPublicKey};

/// RSA-2048: every encrypted or signed block is 256 bytes.
pub const RSA_BLOCK_LEN: usize = 256;
/// Largest PKCS#1 v1.5 payload in one block (256 − 11).
pub const MAX_PAYLOAD_LEN: usize = RSA_BLOCK_LEN - 11;
/// Characters MSI's driver uses for its challenge.
pub const CHALLENGE_ALPHABET: &[u8; 62] =
    b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
/// MSI picks a length in 1..=245; we stay in its range but never go short.
pub const CHALLENGE_LEN: std::ops::RangeInclusive<usize> = 64..=MAX_PAYLOAD_LEN;

#[derive(Debug, thiserror::Error)]
pub enum KeyError {
    #[error("not a PEM public key: {0}")]
    Pem(String),
    #[error("device key must be RSA-2048, got {0} bits")]
    Size(usize),
}

/// The P13's public key, as embedded in MSI's display driver
/// (`AicUsbDisplayDriver.dll`). Every P13 answers to it: the driver carries
/// this single key and the private half lives in the cooler.
pub const P13_PUBLIC_KEY_PEM: &str = include_str!("../keys/p13_public_key.pem");

/// The device's RSA public key (PEM `PUBLIC KEY`, embedded in MSI's driver).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceKey(RsaPublicKey);

impl DeviceKey {
    /// The built-in P13 key ([`P13_PUBLIC_KEY_PEM`]).
    pub fn p13() -> Self {
        Self::from_pem(P13_PUBLIC_KEY_PEM).expect("built-in key is a valid RSA-2048 public key")
    }

    pub fn from_pem(pem: &str) -> Result<Self, KeyError> {
        let key = RsaPublicKey::from_public_key_pem(pem).map_err(|e| KeyError::Pem(e.to_string()))?;
        Self::new(key)
    }

    pub fn new(key: RsaPublicKey) -> Result<Self, KeyError> {
        if key.size() != RSA_BLOCK_LEN {
            return Err(KeyError::Size(key.size() * 8));
        }
        Ok(Self(key))
    }

    /// Auth step 1 payload: `challenge` encrypted for the device (PKCS#1 v1.5).
    pub fn encrypt(&self, rng: &mut impl CryptoRngCore, challenge: &[u8]) -> Result<Vec<u8>, rsa::Error> {
        self.0.encrypt(rng, Pkcs1v15Encrypt, challenge)
    }

    /// Auth step 2: the payload of the device's signed block (PKCS#1 v1.5,
    /// block type 1), recovered with the public key. `None` if the block is
    /// not a valid signature under this key.
    pub fn recover(&self, signature: &[u8]) -> Option<Vec<u8>> {
        if signature.len() != RSA_BLOCK_LEN {
            return None;
        }
        let s = BigUint::from_bytes_be(signature);
        if &s >= self.0.n() {
            return None;
        }
        let m = s.modpow(self.0.e(), self.0.n()).to_bytes_be();
        let mut block = [0u8; RSA_BLOCK_LEN];
        block[RSA_BLOCK_LEN - m.len()..].copy_from_slice(&m);
        unpad_signature(&block).map(<[u8]>::to_vec)
    }
}

/// Strips `00 01 FF…FF 00` (at least 8 `FF`) from a type-1 block.
pub fn unpad_signature(block: &[u8; RSA_BLOCK_LEN]) -> Option<&[u8]> {
    if block[..2] != [0x00, 0x01] {
        return None;
    }
    let sep = 2 + block[2..].iter().position(|&b| b != 0xFF)?;
    if block[sep] != 0x00 || sep - 2 < 8 {
        return None;
    }
    let payload = &block[sep + 1..];
    (!payload.is_empty() && payload.len() <= MAX_PAYLOAD_LEN).then_some(payload)
}

/// A fresh random challenge in MSI's format.
pub fn random_challenge(rng: &mut impl RngCore) -> Vec<u8> {
    let span = (CHALLENGE_LEN.end() - CHALLENGE_LEN.start() + 1) as u32;
    let len = CHALLENGE_LEN.start() + (rng.next_u32() % span) as usize;
    (0..len).map(|_| CHALLENGE_ALPHABET[(rng.next_u32() % 62) as usize]).collect()
}

/// How to authenticate with the device.
// Built once per connection attempt, so the inline 316-byte replay is fine.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug)]
pub enum Auth {
    /// Compute the handshake with the device's public key.
    Computed(DeviceKey),
    /// Replay the host side of a captured session.
    Replay(HandshakeData),
}

impl Auth {
    pub fn kind(&self) -> &'static str {
        match self {
            Auth::Computed(_) => "computed",
            Auth::Replay(_) => "replay",
        }
    }
}

pub const BLOB1_LEN: usize = 256;
pub const FINAL_LEN: usize = 60;
/// Size of `handshake.bin`: `blob1 || final60`.
pub const HANDSHAKE_FILE_LEN: usize = BLOB1_LEN + FINAL_LEN;

/// Device answer to auth step 1 (ASCII, base64-like).
pub const AUTH1_RESPONSE_LEN: usize = 202;
/// Device answer to auth step 2.
pub const AUTH2_RESPONSE_LEN: usize = 256;
/// Size of `expected_responses.bin`: auth1 response || auth2 response.
pub const RESPONSES_FILE_LEN: usize = AUTH1_RESPONSE_LEN + AUTH2_RESPONSE_LEN;

#[derive(Debug, thiserror::Error)]
#[error("expected {expected} bytes, got {actual}")]
pub struct WrongLength {
    pub expected: usize,
    pub actual: usize,
}

fn check_len(bytes: &[u8], expected: usize) -> Result<(), WrongLength> {
    if bytes.len() == expected {
        Ok(())
    } else {
        Err(WrongLength { expected, actual: bytes.len() })
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct HandshakeData {
    pub blob1: [u8; BLOB1_LEN],
    pub final60: [u8; FINAL_LEN],
}

impl HandshakeData {
    /// Parses the 316-byte contents of `handshake.bin`.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, WrongLength> {
        check_len(bytes, HANDSHAKE_FILE_LEN)?;
        let (blob1, final60) = bytes.split_at(BLOB1_LEN);
        Ok(Self {
            blob1: blob1.try_into().unwrap(),
            final60: final60.try_into().unwrap(),
        })
    }
}

// Keep the blob out of logs.
impl fmt::Debug for HandshakeData {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("HandshakeData { .. }")
    }
}

/// What the device answered during the handshake. These were identical across
/// sessions in captures, so comparing them is a useful diagnostic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthResponses {
    pub auth1: Vec<u8>,
    pub auth2: Vec<u8>,
}

impl AuthResponses {
    /// Parses the 458-byte contents of `expected_responses.bin`.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, WrongLength> {
        check_len(bytes, RESPONSES_FILE_LEN)?;
        let (auth1, auth2) = bytes.split_at(AUTH1_RESPONSE_LEN);
        Ok(Self { auth1: auth1.to_vec(), auth2: auth2.to_vec() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_handshake_file() {
        let mut bytes = vec![1u8; BLOB1_LEN];
        bytes.extend([2u8; FINAL_LEN]);
        let data = HandshakeData::from_bytes(&bytes).unwrap();
        assert_eq!(data.blob1, [1; BLOB1_LEN]);
        assert_eq!(data.final60, [2; FINAL_LEN]);
    }

    #[test]
    fn rejects_wrong_lengths() {
        let err = HandshakeData::from_bytes(&[0; 315]).unwrap_err();
        assert_eq!((err.expected, err.actual), (316, 315));
        assert!(AuthResponses::from_bytes(&[0; 459]).is_err());
    }

    #[test]
    fn splits_responses_file() {
        let mut bytes = vec![b'A'; AUTH1_RESPONSE_LEN];
        bytes.extend([0x5A; AUTH2_RESPONSE_LEN]);
        let r = AuthResponses::from_bytes(&bytes).unwrap();
        assert_eq!(r.auth1.len(), 202);
        assert_eq!(r.auth2, vec![0x5A; 256]);
    }

    #[test]
    fn challenges_use_msi_format() {
        let mut rng = rand_core::OsRng;
        for _ in 0..50 {
            let c = random_challenge(&mut rng);
            assert!(CHALLENGE_LEN.contains(&c.len()), "{}", c.len());
            assert!(c.iter().all(|b| CHALLENGE_ALPHABET.contains(b)));
        }
        assert_ne!(random_challenge(&mut rng), random_challenge(&mut rng));
    }

    #[test]
    fn unpads_signature_blocks() {
        let mut block = [0xFFu8; RSA_BLOCK_LEN];
        block[0] = 0;
        block[1] = 1;
        block[RSA_BLOCK_LEN - 61] = 0;
        block[RSA_BLOCK_LEN - 60..].copy_from_slice(&[b'7'; 60]);
        assert_eq!(unpad_signature(&block), Some(&[b'7'; 60][..]));

        let mut wrong_type = block;
        wrong_type[1] = 2;
        assert_eq!(unpad_signature(&wrong_type), None);

        let mut no_separator = block;
        no_separator[RSA_BLOCK_LEN - 61] = 0xFF;
        no_separator[RSA_BLOCK_LEN - 60..].fill(0xFF);
        assert_eq!(unpad_signature(&no_separator), None);

        let mut short_padding = [0u8; RSA_BLOCK_LEN];
        short_padding[1] = 1;
        short_padding[2..6].fill(0xFF); // only 4 FF before the separator
        short_padding[7..].fill(b'x');
        assert_eq!(unpad_signature(&short_padding), None);
    }

    #[test]
    fn built_in_key_is_valid() {
        assert_eq!(DeviceKey::p13().0.size(), RSA_BLOCK_LEN);
    }

    #[test]
    fn rejects_non_2048_keys() {
        let pem = "-----BEGIN PUBLIC KEY-----\nnot a key\n-----END PUBLIC KEY-----\n";
        assert!(matches!(DeviceKey::from_pem(pem), Err(KeyError::Pem(_))));
    }

    #[test]
    fn debug_hides_blob() {
        let data = HandshakeData { blob1: [0xAB; BLOB1_LEN], final60: [0xCD; FINAL_LEN] };
        assert_eq!(format!("{data:?}"), "HandshakeData { .. }");
    }
}
