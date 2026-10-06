# P13 display handshake (interface 0)

Reverse engineered from MSI's `AicUsbDisplayDriver.dll` (Ghidra, function
`FUN_1800f70d4`) and checked against `research/coldstart.pcapng`. The binary
and decompiled output stay in `research/` and are not committed.

## Summary

A mutual RSA-2048 challenge-response. The host only needs the device's
**public** key, which is embedded in the driver as a PEM `PUBLIC KEY`
(`e = 65537`). Nothing secret is involved on the host side, so `blob1` and
`final60` do not have to be stored: both are produced fresh each session.

## Steps (bulk messages, same 20-byte header as before)

1. **Host challenge.** Random string, length uniform in 1..245, characters
   from `0-9A-Za-z` (MSI seeds an `mt19937` from the clock). 245 = 256 − 11,
   the largest PKCS#1 v1.5 plaintext for a 2048-bit key.
2. **Auth 1 (kind `0x10`, length 256).** Payload = challenge encrypted with
   the public key, RSA PKCS#1 v1.5 (block type 2). This is what `blob1` was.
3. **Device answer (bulk IN).** The challenge in plain text. The driver
   checks length and bytes match exactly. (The captured 202-byte "base64"
   response is simply MSI's 202-character challenge.)
4. **Auth 2 (kind `0x11`, length 256, no payload).**
5. **Device answer (bulk IN, 256 bytes).** An RSA signature (PKCS#1 v1.5,
   block type 1) made with the device's private key.
6. **Host reply (raw bulk OUT, no header).** Apply the public key
   (`s^e mod n`), check the `00 01 FF … FF 00` padding, and send the payload
   that follows. The driver rejects payloads over 245 bytes. This is what
   `final60` was.

Verified offline: applying the embedded public key to the captured step-5
answer yields correctly padded type-1 data whose payload equals the
captured `final60` byte for byte.

## Implementation

- `aio-proto`: `DeviceKey` (encrypt / recover), `random_challenge`, and
  `Auth::{Computed, Replay}` passed to `Display::connect`. The mock device
  plays the private-key side with a throwaway test key.
- The key lives in `%ProgramData%\aio-ui\device_key.pem`, written by
  `scripts/extract_device_key.py` from the installed driver (or the copy in
  `research/`). It is not secret, but it comes from MSI's driver, so it is
  git-ignored like the other extracted data.
- The daemon and `aio-show` use the computed handshake when the key exists.
  If it fails during authentication and `handshake.bin` exists, the daemon
  falls back to the replay for later attempts; `aio-show --replay` forces it.
