# P13 HID control protocol (interface 1)

Settings such as brightness go over the HID interface (interface 1, usage
page `0xFF00`), not the display interface. Sources: USB captures
(`research/on_off.pcapng`, `brightness.pcapng`, `coldstart.pcapng`) and the
decompiled MSI EZ Display client (`research/msi/src`, class
`Class_P13_Coreliquid`). Neither source is committed.

## Transport

- One request = one 1024-byte HID OUT report on endpoint `0x03` (on Windows:
  `WriteFile` of 1025 bytes, report ID `0x00` first). Zero-padded.
- One response = one 1024-byte HID IN report on endpoint `0x82`.
- MSI sends most requests twice and waits up to 3 s for the matching
  response; there is ~10 ms between writes.

## Framing

```
5A | length (u16, big-endian) | message (ASCII) | checksum (u8) | 5A
```

- `length` = size of the whole unescaped frame, including both `5A` bytes and
  the checksum.
- `checksum` = sum of the two length bytes and the message bytes, mod 256.
- Escaping, applied to everything between the delimiters (length, message
  and checksum): `5A` → `5B 01`, `5B` → `5B 02`. The first was seen in a
  capture; both are in the escape table that MSI's native `BYProtocol.dll`
  sets up at load time (decompiled with Ghidra).

Verified byte for byte against every captured request and response.

## Messages

Request:
```
POST <cmd> 1\r\n
SeqNumber=<n>\r\n
ContentType=json\r\n
ContentLength=<body length>\r\n
\r\n
<json body>
```
Without a body the request ends after `ContentType=json` (no blank line).

Response: `1 200\r\nAckNumber=<n+1>\r\n` optionally followed by
`ContentType=json`, `ContentLength=` and a JSON body after a blank line.

## Commands

| cmd | SeqNumber | Body | Used by MSI | Notes |
|---|---|---|---|---|
| `conn` | 100 | none | at connect | Returns device info (below). Read-only. |
| `brightness` | 100 | `{"value":0..100}` | yes | Backlight. 0 is MSI's "LCD off" and stealth mode. **Persists across power cycles.** |
| `rotate` | 100 | `{"degree":N}` | yes | Screen rotation; device default 180 (also the `0xB4` in the USB device info). |
| `extendedDisplay` | 110 | `{"enable":bool}` | yes | MSI sets `false` on Windows shutdown, `true` at startup and on resume. Exact effect on the panel not yet observed. |
| `realtimeDisplay` | 112 | `{"enable":bool}` | no (dead code) | Paired with an HID image transfer that is also unused. |
| `transport` / `transported` | 102 / 104 | `{"type","fileName","fileSize"}` / `{"fileName","md5"}` | no (dead code) | Uploads a file to device storage in 1000-byte HID blocks. Writes flash. |
| `reboot` | 106 | `{"enable":true}` | no (dead code) | Reboots the controller. |
| `upgrade` | 108 | `{"enable":true}` | no (dead code) | **Firmware upgrade. Never send.** |

`conn` response body (serial number omitted):

```json
{"OS":"RTOS","Manufacturer":"MSI","model":"MPG CORELIQUID P13 Series",
 "version":{"app":"V1.0.11","firmware":"P13_20251204v01","sdk":"V1.2.0","hardware":"V2.0"},
 "brightness":100,"degree":180,"sn":"…","bootFinish":1,"realtimeDisplay":0,"extendedDisplay":1}
```

## Safety policy for this project

Only `conn`, `brightness`, `rotate` and `extendedDisplay` may be sent. The
other commands stay blocked in code: they are unused by MSI's own software,
write to flash, or start a firmware upgrade.

## Implementation

- `aio_proto::hid`: framing, the `Command` enum (the four commands above are
  the only ones it can express), response parsing, and `HidClient` (flush,
  send, wait up to 3 s for the matching `AckNumber`, send once more if
  unanswered). `mock::MockHid` simulates the firmware for tests.
- `aio-daemon/src/hid.rs`: Windows HID transport (shared access, 1025-byte
  reports with report ID 0, overlapped I/O with timeouts).
- Daemon behaviour: on connect `conn` → `extendedDisplay` on if it was off
  (as MSI does) → rotation if configured → brightness; on shutdown and before
  sleep, after the fade to black, `brightness 0`. HID failures disable HID
  control until the next connect; the display keeps working.
- Settings: `aio-cli brightness <0-100>`, `aio-cli rotate <0|90|180|270>`,
  stored in `config.toml`. `aio-show --info` / `--brightness N` talk to HID
  only, for testing.
