//! Files under `%ProgramData%\aio-ui`.

use std::path::{Path, PathBuf};

use aio_proto::{Auth, AuthResponses, DeviceKey, HandshakeData};
use anyhow::{Context, Result};

pub fn data_dir() -> PathBuf {
    let base = std::env::var_os("ProgramData").unwrap_or_else(|| r"C:\ProgramData".into());
    PathBuf::from(base).join("aio-ui")
}

/// Daily log files when running as a service.
pub fn log_dir() -> PathBuf {
    data_dir().join("logs")
}

pub fn config_file() -> PathBuf {
    data_dir().join("config.toml")
}

/// Pre-encoded frames, one directory per imported source.
pub fn cache_dir() -> PathBuf {
    data_dir().join("cache")
}

/// The device's RSA public key, extracted from MSI's driver by
/// `scripts/extract_device_key.py`. With it the handshake is computed.
pub fn device_key_file() -> PathBuf {
    data_dir().join("device_key.pem")
}

/// `blob1 || final60`, extracted from a capture by `scripts/extract_handshake.py`.
pub fn handshake_file() -> PathBuf {
    data_dir().join("handshake.bin")
}

/// Optional: the device answers seen in the capture, for diagnostics.
pub fn expected_responses_file() -> PathBuf {
    data_dir().join("expected_responses.bin")
}

pub fn load_device_key(path: &Path) -> Result<DeviceKey> {
    let pem = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    DeviceKey::from_pem(&pem).with_context(|| format!("invalid {}", path.display()))
}

/// The computed handshake if `key` exists, else the replay of `handshake`.
/// `prefer_replay` picks the replay when both exist (fallback after a failure).
pub fn load_auth(key: &Path, handshake: &Path, prefer_replay: bool) -> Result<Auth> {
    if key.is_file() && !(prefer_replay && handshake.is_file()) {
        load_device_key(key).map(Auth::Computed)
    } else {
        load_handshake(handshake)
            .map(Auth::Replay)
            .with_context(|| format!("no device key at {}", key.display()))
    }
}

pub fn load_handshake(path: &Path) -> Result<HandshakeData> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    HandshakeData::from_bytes(&bytes).with_context(|| format!("invalid {}", path.display()))
}

/// Returns `Ok(None)` if the file doesn't exist.
pub fn load_expected_responses(path: &Path) -> Result<Option<AuthResponses>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(
            AuthResponses::from_bytes(&bytes).with_context(|| format!("invalid {}", path.display()))?,
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(dir: &Path, key: bool, replay: bool) -> (PathBuf, PathBuf) {
        let (k, h) = (dir.join("device_key.pem"), dir.join("handshake.bin"));
        if key {
            std::fs::write(&k, aio_proto::mock::mock_device_key_pem()).unwrap();
        }
        if replay {
            std::fs::write(&h, [0u8; 316]).unwrap();
        }
        (k, h)
    }

    #[test]
    fn prefers_key_then_replay() {
        let tmp = tempfile::tempdir().unwrap();
        let (k, h) = files(tmp.path(), true, true);
        assert!(matches!(load_auth(&k, &h, false).unwrap(), Auth::Computed(_)));
        assert!(matches!(load_auth(&k, &h, true).unwrap(), Auth::Replay(_)));
    }

    #[test]
    fn uses_whatever_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let (k, h) = files(tmp.path(), true, false);
        assert!(matches!(load_auth(&k, &h, true).unwrap(), Auth::Computed(_)));

        let tmp = tempfile::tempdir().unwrap();
        let (k, h) = files(tmp.path(), false, true);
        assert!(matches!(load_auth(&k, &h, false).unwrap(), Auth::Replay(_)));

        let tmp = tempfile::tempdir().unwrap();
        let (k, h) = files(tmp.path(), false, false);
        let err = load_auth(&k, &h, false).unwrap_err();
        assert!(format!("{err:#}").contains("no device key"), "{err:#}");
    }

    /// Checks the computed handshake against the captured session, if the
    /// extracted files are present on this machine (they are never committed).
    #[test]
    fn device_key_recovers_captured_final_block() {
        let (key, hs, exp) = (device_key_file(), handshake_file(), expected_responses_file());
        if !(key.is_file() && hs.is_file() && exp.is_file()) {
            eprintln!("extracted handshake files not present; skipping");
            return;
        }
        let key = load_device_key(&key).unwrap();
        let replay = load_handshake(&hs).unwrap();
        let captured = load_expected_responses(&exp).unwrap().unwrap();
        assert_eq!(key.recover(&captured.auth2).as_deref(), Some(&replay.final60[..]));
    }
}
