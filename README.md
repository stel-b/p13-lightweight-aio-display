# AIO Display for the MSI MPG CoreLiquid P13

A lightweight replacement for MSI Center / EZ Display that drives the 2.1"
480×480 LCD on the pump of the **MSI MPG CoreLiquid P13** AIO cooler directly
over USB, without the virtual "third monitor" MSI's software creates.

It shows a color, an image, a GIF or a video on the pump screen, controls
brightness and rotation, fades out and turns the backlight off on shutdown
and sleep, and runs as a small Windows service.

## Why this exists

Two things about MSI's software made me want a replacement:

- **The screen is added to Windows as an extra monitor.** MSI's driver turns
  the pump LCD into a third display. The mouse cursor wanders onto a screen
  you can't see, windows and games pick the wrong monitor, and some games
  misbehave or refuse to start (Black Ops 2 Zombies would not launch). This
  project talks to the LCD directly over USB, so Windows never sees a monitor.
- **MSI Center is heavy for what it does.** Showing a picture on a small
  screen takes several background services, a full desktop app with a web
  view and a media player, and that virtual display. I wanted it to cost
  next to nothing.

So the whole design aims at being as light as possible:

- Images, GIFs and videos are converted **once**, when you choose them, into
  ready-to-send frames on disk. Playing them back is just "read frame, send
  over USB, wait": no decoding or encoding while running.
- A still image or color is sent once; after that the service sleeps.
- The service is a small native program. Measured on the author's PC while
  playing a 24 fps video: **about 2 MB of private memory**, and CPU use too
  low for Windows' performance counters to register.
- The settings window only exists while it is open (closing it exits the
  process), and the optional tray icon is a separate ~2 MB process.

> [!WARNING]
> **Read this before using it.**
>
> - **This project was written mostly by an AI** (Anthropic's Claude), guided
>   and tested by a human. It was reverse engineered from USB captures and
>   MSI's own software; it is **not** an official or supported tool.
> - **Use it at your own risk.** It replaces a USB driver, talks to the
>   cooler's display controller with commands that were worked out by
>   observation, and installs a Windows service that runs as LocalSystem.
>   Nothing here has been tested beyond one machine.
> - It works on the author's P13 (firmware `P13_20251204v01`), which runs it
>   daily. **Other coolers, revisions or firmware versions may behave
>   differently.** Only use it if you understand what the steps below do and
>   know how to undo them.
> - The pump is driven by the motherboard's fan header, not by this software,
>   so cooling does not depend on it. The display controller, however, can be
>   put into odd states; a full power-off of the PC has always recovered it.
> - Not affiliated with or endorsed by MSI or ArtInChip. Product names are
>   trademarks of their owners.

## Components

The repository is a Rust workspace. Programs end up in
`C:\Program Files\aio-ui` after installation.

| Crate | Programs | What it does |
|---|---|---|
| `aio-proto` | (library) | The USB protocol, with no USB dependency: the display handshake (computed RSA challenge-response), JPEG frame packaging, and the HID control channel (brightness, rotation, device info). Includes a simulated device used by the tests. |
| `aio-daemon` | `aio-daemon.exe`, `aio-show.exe` | The background service. It owns the USB connection, reconnects automatically, plays the configured source from a cache of pre-encoded frames (so playback costs almost no CPU), handles device removal, sleep and shutdown (fade to black, backlight off), and serves the other programs over a named pipe. `aio-show` is a one-shot test tool. |
| `aio-ipc` | (library) | The messages between the daemon and its clients. |
| `aio-cli` | `aio-cli.exe` | Command-line control: set a source, manage the library, pause/resume, brightness, rotation, status, save a preview. |
| `aio-ui` | `aio-ui.exe` | The settings window (status, live preview, source picker, library, brightness, rotation). `aio-ui --tray` is a tiny tray icon (~2 MB) that opens the window on demand. |
| `aio-loop` | `aio-loop.exe`, `aio-loop-cli.exe` | **Loop finder**, a standalone tool: open a video, mark roughly where a loop should start and end, and it finds frames that join seamlessly and saves `<name>_loop.mp4`. |

Documentation of the reverse-engineered protocol and the loop finder:

- [docs/handshake.md](docs/handshake.md): the display handshake
- [docs/hid-protocol.md](docs/hid-protocol.md): the HID control channel
- [docs/loop-finder.md](docs/loop-finder.md): how loops are found and cut

### How it works, briefly

The cooler is a composite USB device (`33C3:0E02`). **Interface 0** carries the
display: after a handshake, JPEG frames are sent over a bulk endpoint. To use
it without MSI's driver, interface 0 is switched to Windows' generic **WinUSB**
driver. **Interface 1** is a standard HID device that accepts small text
commands (`POST brightness 1`, etc.); it keeps its normal Windows driver.

Only a small, explicit set of commands is ever sent (see the protocol docs).
Commands found in MSI's software that write to flash or update firmware are
deliberately not implemented.

## Installation

### 1. Prepare the cooler

These steps are needed however you install.

1. **Remove MSI's software.** Uninstall MSI Center / EZ Display, or at least
   stop its services: they hold the device. Steam (Steam Input) can grab it
   too; once the service is installed it claims the display at boot, before
   Steam starts.
2. **Switch interface 0 to WinUSB** with [Zadig](https://zadig.akeo.ie):
   1. In **Options**, enable **List All Devices**.
   2. Select **P13 (Interface 0)**. Do **not** select interface 1 or the
      composite device.
   3. Choose **WinUSB** and click **Replace Driver**.

   The virtual third monitor disappears.
3. **Optional, for videos:** install [ffmpeg](https://ffmpeg.org), for example
   `winget install Gyan.FFmpeg`. Images, GIFs and colors work without it.

### 2. Install

#### With the installer (recommended)

Download `aio-display-setup-<version>.exe` from the
[Releases](../../releases) page and run it. It shows the warnings above and
the Zadig steps again, then:

- copies the programs to `C:\Program Files\aio-ui`,
- registers and starts the `aio-daemon` service (automatic start, restart on
  failure),
- adds **AIO Display** and **AIO Loop Finder** to the Start Menu,
- optionally shows the tray icon at login,
- with **Video support** ticked, tells the service where ffmpeg is. If ffmpeg
  (and ffprobe) cannot be found, setup stops with an error so you can install
  it first; untick the option to install without video support.

To update, run a newer installer over the old one. To uninstall, use
Settings > Apps.

The installer and programs are not code-signed, so Windows SmartScreen will
warn ("Windows protected your PC" > More info > Run anyway). They are built by
GitHub Actions from this repository's source
([release workflow](.github/workflows/release.yml)).

Each release also has a **portable zip** with the same programs and
`install.ps1` (see below) for people who prefer a script.

#### From source

Requirements: [Rust](https://rustup.rs) with the MSVC toolchain and the
[Visual Studio Build Tools](https://visualstudio.microsoft.com/visual-cpp-build-tools/)
("Desktop development with C++").

```powershell
cargo build --release

# Try it before installing (with Steam and MSI software closed):
.\target\release\aio-show.exe red          # the pump display turns red
.\target\release\aio-show.exe --info       # prints device info (HID)

# Install, from an Administrator PowerShell:
.\scripts\install.ps1 -VideoSupport -Autostart
```

[scripts/install.ps1](scripts/install.ps1) does the same as the installer and
every step is commented. `-VideoSupport` stops before changing anything if
ffmpeg is not found; `-Autostart` adds the tray icon at login. Run it again
after rebuilding to update; `scripts\uninstall.ps1` removes it.

Settings, the frame cache and logs live in `C:\ProgramData\aio-ui` either
way.

### About the device key

The display handshake needs the cooler's RSA **public** key. It comes from
MSI's display driver (`AicUsbDisplayDriver.dll`) and is built into the
programs (`crates/aio-proto/keys/p13_public_key.pem`): the driver carries this
one key for all P13s, and the private half stays inside the cooler. Nothing
has to be extracted. Should MSI ever ship a different key, run
`python scripts\extract_device_key.py` while their driver is installed; it
writes `C:\ProgramData\aio-ui\device_key.pem`, which overrides the built-in
key. See [docs/handshake.md](docs/handshake.md).

## Usage

- **AIO Display** (Start Menu): status, live preview, choose a color, image,
  GIF or video, pause/resume, brightness, rotation.
- **Library**: add GIFs, videos and images once (they are imported then, so
  switching later is instant) and pick between them. With **At startup show:
  Random item**, the daemon picks a different random item each time it starts,
  so each boot shows something new. Removing an item never deletes your file.
- **AIO Loop Finder** (Start Menu): make a seamless loop from a video, then
  show it on the pump directly.
- Command line:

  ```powershell
  aio-cli set color teal
  aio-cli set gif   C:\path\anim.gif
  aio-cli set video C:\path\clip.mp4
  aio-cli brightness 60
  aio-cli rotate 180
  aio-cli pause | resume | status
  aio-cli library add C:\path\a.gif C:\path\b.mp4
  aio-cli library                # list; * marks the item on the display
  aio-cli library show 2
  aio-cli library remove 2
  aio-cli mode random            # or: mode selected
  aio-loop-cli auto C:\path\clip.mp4 0:03 2:10
  ```

Images, GIFs and videos are center-cropped to a square and scaled to 480×480.

## Uninstalling / going back to MSI

1. Uninstall **AIO Display for MSI P13** in Settings > Apps (if you used
   `install.ps1`: run `.\scripts\uninstall.ps1` as Administrator). This
   removes the service, programs and shortcuts and keeps
   `C:\ProgramData\aio-ui`.
2. In Device Manager, uninstall **P13 (Interface 0)** and tick
   **"Attempt to remove the driver for this device"** (or remove the Zadig
   driver with `pnputil /delete-driver oemNN.inf /uninstall`).
3. Reinstall MSI Center / EZ Display.

## Troubleshooting

- **"device is in use by another program"**: MSI's services may have it
  open. Quit them. If that doesn't work try a reboot.
- **insufficient permissions**: It's not needed to run any software here as admin, except for the install script. Usually this just means you need to reboot and try again.
- **Logs**: `C:\ProgramData\aio-ui\logs` (service) or the console output of
  `aio-daemon.exe` run in the foreground.
- **Videos don't import** ("video support is not set up"): install ffmpeg,
  then run the installer again with **Video support** ticked (from source:
  `install.ps1 -VideoSupport`).
- **Display stuck or blank**: a full power-off of the PC (not a restart)
  resets the display controller.

## Development

```powershell
cargo test --workspace
```

Almost everything is tested without hardware against the simulated device.
Tests that need ffmpeg or local USB captures skip themselves when those are
missing. `crates/aio-proto/src/mock_device_key.pem` is a **throwaway key
generated for the tests**; it has nothing to do with the real device.

`research/` (USB captures, MSI binaries, decompiled code) is git-ignored and
never part of the repository. `scripts/ghidra/` holds the helper used to
decompile MSI's native DLLs with Ghidra.

## Credits

Written mostly by Claude (Anthropic) in an AI pair-programming session, with a
human doing the hardware testing, captures and decisions.
