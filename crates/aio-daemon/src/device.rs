//! The device thread: owns the USB connection and the player.
//!
//! It blocks on its command channel between frames, so commands take effect
//! immediately even mid-animation. On any USB error it drops the connection,
//! waits with exponential backoff (cut short when the device reappears),
//! reconnects, and resends the current frame. When Windows asks for the
//! device (disable/removal), it closes the handle and waits for the device to
//! come back. On shutdown, and before the system sleeps, it fades the
//! display to black and turns the backlight off.
//!
//! It also owns the HID control interface (brightness, rotation). HID is
//! optional: if it fails, the display keeps working without it.

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use aio_ipc::DeviceState;
use aio_proto::hid::{Command as HidCommand, HidClient, HidTransport, Rotation};
use aio_proto::{Auth, Display, Transport};
use serde::Deserialize;
use tracing::{debug, info, warn};

use crate::media::CachedMedia;
use crate::{paths, render};
use crate::player::{PlayError, Player};
use crate::hid::HidDevice;
use crate::notify::NotifyTarget;
use crate::winusb::{OpenError, WinUsbTransport};

pub enum Command {
    SetMedia(CachedMedia),
    Pause,
    Resume,
    /// The display interface appeared (plugged in, re-enabled, re-enumerated).
    DeviceArrived,
    /// Drop the connection and reconnect (device removed, system resumed).
    Reconnect(&'static str),
    /// Windows wants to disable or remove the device: close the handle, then
    /// acknowledge. No reconnect until the device arrives again.
    Release(SyncSender<()>),
    /// The system is about to sleep: fade to black, acknowledge, and send
    /// nothing more until resumed (a [`Command::Reconnect`] follows).
    FadeOut(SyncSender<()>),
    /// New backlight level (0..=100), applied now if connected.
    SetBrightness(u8),
    /// New rotation, applied now if connected.
    SetRotation(Rotation),
    /// Fade to black, turn the backlight off, then stop.
    Shutdown,
}

/// Settings sent over HID at every connect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceSettings {
    pub brightness: u8,
    /// `None`: leave whatever the device has (its default is 180 degrees).
    pub rotation: Option<Rotation>,
}

impl Default for DeviceSettings {
    fn default() -> Self {
        Self { brightness: 100, rotation: None }
    }
}

/// The parts of the `conn` answer we use.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct DeviceInfo {
    version: Version,
    degree: u16,
    extended_display: u8,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Version {
    app: String,
    firmware: String,
}

/// How the worker gets a connection; swapped for a mock in tests.
pub trait Connector: Send + 'static {
    type Transport: Transport;
    type Hid: HidTransport;
    /// Credentials for the next connection attempt.
    fn auth(&mut self) -> anyhow::Result<Auth>;
    /// The last attempt failed during authentication with `auth`.
    fn on_auth_failed(&mut self, _auth: &Auth) {}
    fn open(&mut self) -> Result<Self::Transport, OpenError>;
    /// The HID control interface (brightness, rotation).
    fn open_hid(&mut self) -> Result<Self::Hid, OpenError>;
    /// Called just before a transport is dropped because of [`Command::Release`].
    fn on_release(&mut self, _transport: &mut Self::Transport) {}
}

pub struct UsbConnector {
    /// `device_key.pem`: preferred, the handshake is computed from it.
    pub key_path: PathBuf,
    /// `handshake.bin`: replayed if there is no key, or the key failed.
    pub handshake_path: PathBuf,
    pub notify: Option<NotifyTarget>,
    /// Set after the computed handshake failed and a replay file exists.
    pub use_replay: bool,
}

impl UsbConnector {
    pub fn new(key_path: PathBuf, handshake_path: PathBuf, notify: Option<NotifyTarget>) -> Self {
        Self { key_path, handshake_path, notify, use_replay: false }
    }
}

impl Connector for UsbConnector {
    type Transport = WinUsbTransport;
    type Hid = HidDevice;

    // Re-read on every attempt, so dropping a file in place fixes a missing one.
    fn auth(&mut self) -> anyhow::Result<Auth> {
        paths::load_auth(&self.key_path, &self.handshake_path, self.use_replay)
    }

    fn on_auth_failed(&mut self, auth: &Auth) {
        if matches!(auth, Auth::Computed(_)) && self.handshake_path.is_file() && !self.use_replay {
            warn!("computed handshake failed; falling back to the replayed handshake");
            self.use_replay = true;
        }
    }

    fn open(&mut self) -> Result<WinUsbTransport, OpenError> {
        WinUsbTransport::open(self.notify)
    }

    fn open_hid(&mut self) -> Result<HidDevice, OpenError> {
        HidDevice::open()
    }

    fn on_release(&mut self, transport: &mut WinUsbTransport) {
        // The notification window unregisters it while answering Windows.
        transport.forget_notification();
    }
}

/// State the worker publishes for status queries.
pub struct Shared {
    pub device: DeviceState,
    pub paused: bool,
    pub frames_sent: u64,
    pub last_error: Option<String>,
    /// The JPEG currently on the display.
    pub preview: Vec<u8>,
    /// From the device's `conn` answer, when HID control works.
    pub firmware: Option<String>,
    fps: f32,
    last_send: Option<Instant>,
}

impl Shared {
    pub fn new(paused: bool) -> Self {
        Self {
            device: DeviceState::Disconnected,
            paused,
            frames_sent: 0,
            last_error: None,
            preview: Vec::new(),
            firmware: None,
            fps: 0.0,
            last_send: None,
        }
    }

    /// Frames per second over the last measured second; 0 when idle.
    pub fn fps(&self, now: Instant) -> f32 {
        match self.last_send {
            Some(t) if now.saturating_duration_since(t) < Duration::from_millis(1500) => self.fps,
            _ => 0.0,
        }
    }
}

pub fn lock(shared: &Mutex<Shared>) -> MutexGuard<'_, Shared> {
    shared.lock().unwrap_or_else(|e| e.into_inner())
}

#[derive(Debug, Clone, Copy)]
pub struct Timing {
    pub min: Duration,
    pub max: Duration,
    /// After releasing the device to Windows, don't try to reopen it for this
    /// long unless it arrives again (so we don't grab it mid-removal).
    pub release_hold: Duration,
    /// Length of the fade to black on shutdown and sleep.
    pub fade: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            min: Duration::from_millis(500),
            max: Duration::from_secs(15),
            release_hold: Duration::from_secs(5),
            fade: Duration::from_millis(800),
        }
    }
}

enum Exit {
    Shutdown,
    Lost,
    Released,
}

enum Effect {
    /// Send the current frame right away.
    SendNow,
    Pause,
    Arrived,
    Reconnect(&'static str),
    Release(SyncSender<()>),
    FadeOut(SyncSender<()>),
    /// A setting changed: send this request if connected.
    Hid(HidCommand),
    Shutdown,
}

/// Frames in a fade to black (30 fps over the default 800 ms).
const FADE_STEPS: u32 = 24;

struct FpsMeter {
    window_start: Instant,
    frames: u32,
}

pub struct Worker<C: Connector> {
    connector: C,
    rx: Receiver<Command>,
    shared: Arc<Mutex<Shared>>,
    player: Option<Player>,
    paused: bool,
    settings: DeviceSettings,
    hid: Option<HidClient<C::Hid>>,
    timing: Timing,
    meter: FpsMeter,
}

impl<C: Connector> Worker<C> {
    pub fn new(
        connector: C,
        rx: Receiver<Command>,
        shared: Arc<Mutex<Shared>>,
        media: Option<CachedMedia>,
        paused: bool,
        settings: DeviceSettings,
        timing: Timing,
    ) -> Self {
        Self {
            connector,
            rx,
            shared,
            player: media.map(Player::new),
            paused,
            settings,
            hid: None,
            timing,
            meter: FpsMeter { window_start: Instant::now(), frames: 0 },
        }
    }

    /// Runs until [`Command::Shutdown`] or the sender is dropped.
    pub fn run(mut self) {
        let mut delay = self.timing.min;
        loop {
            match self.connect() {
                Some(display) => {
                    delay = self.timing.min;
                    let pause = match self.serve(display) {
                        Exit::Shutdown => return,
                        // Give a crashed or re-enumerating device a moment.
                        Exit::Lost => self.timing.min,
                        Exit::Released => self.timing.release_hold,
                    };
                    if !self.wait(pause) {
                        return;
                    }
                }
                None => {
                    if !self.wait(delay) {
                        return;
                    }
                    delay = (delay * 2).min(self.timing.max);
                }
            }
        }
    }

    /// Records a state change; logs only when the error message changes.
    fn report(&mut self, device: DeviceState, error: Option<String>) {
        let mut s = lock(&self.shared);
        if error.is_some() && error != s.last_error {
            warn!("{}", error.as_deref().unwrap_or_default());
        } else if let Some(e) = &error {
            debug!("{e}");
        }
        s.device = device;
        if error.is_some() || device == DeviceState::Connected {
            s.last_error = error;
        }
    }

    fn connect(&mut self) -> Option<Display<C::Transport>> {
        let auth = match self.connector.auth() {
            Ok(a) => a,
            Err(e) => {
                self.report(DeviceState::Disconnected, Some(format!("{e:#}")));
                return None;
            }
        };
        let transport = match self.connector.open() {
            Ok(t) => t,
            Err(e @ OpenError::Busy(_)) => {
                self.report(DeviceState::Busy, Some(e.to_string()));
                return None;
            }
            Err(e) => {
                self.report(DeviceState::Disconnected, Some(e.to_string()));
                return None;
            }
        };
        match Display::connect(transport, &auth) {
            Ok(display) => {
                info!(handshake = auth.kind(), "display connected");
                self.report(DeviceState::Connected, None);
                self.setup_hid();
                Some(display)
            }
            Err(e) => {
                if e.is_auth_failure() {
                    self.connector.on_auth_failed(&auth);
                }
                let msg = format!("{} handshake failed: {e}", auth.kind());
                self.report(DeviceState::Disconnected, Some(msg));
                None
            }
        }
    }

    /// Applies a command's effect on playback state; the caller decides
    /// what it means for the connection.
    fn apply(&mut self, cmd: Command) -> Effect {
        match cmd {
            Command::SetMedia(media) => {
                info!(key = media.key(), frames = media.frames().len(), "new source");
                self.player = Some(Player::new(media));
                Effect::SendNow
            }
            Command::Pause => {
                self.paused = true;
                lock(&self.shared).paused = true;
                Effect::Pause
            }
            Command::Resume => {
                self.paused = false;
                lock(&self.shared).paused = false;
                if let Some(p) = &mut self.player {
                    p.resume();
                }
                Effect::SendNow
            }
            Command::DeviceArrived => Effect::Arrived,
            Command::Reconnect(reason) => Effect::Reconnect(reason),
            Command::Release(ack) => Effect::Release(ack),
            Command::FadeOut(ack) => Effect::FadeOut(ack),
            Command::SetBrightness(value) => {
                self.settings.brightness = value.min(100);
                Effect::Hid(HidCommand::Brightness(self.settings.brightness))
            }
            Command::SetRotation(rotation) => {
                self.settings.rotation = Some(rotation);
                Effect::Hid(HidCommand::Rotate(rotation))
            }
            Command::Shutdown => Effect::Shutdown,
        }
    }

    /// Waits while disconnected. Returns false on shutdown, true when it is
    /// time to try connecting again.
    fn wait(&mut self, duration: Duration) -> bool {
        let deadline = Instant::now() + duration;
        loop {
            let cmd = match self.rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(cmd) => cmd,
                Err(RecvTimeoutError::Timeout) => return true,
                Err(RecvTimeoutError::Disconnected) => return false,
            };
            match self.apply(cmd) {
                Effect::Shutdown => return false,
                Effect::Arrived => return true,
                // Nothing is open: Windows can have it, and nothing to fade.
                Effect::Release(ack) | Effect::FadeOut(ack) => {
                    let _ = ack.send(());
                }
                // Settings are applied at the next connect.
                Effect::SendNow | Effect::Pause | Effect::Reconnect(_) | Effect::Hid(_) => {}
            }
        }
    }

    /// Plays while connected.
    fn serve(&mut self, mut display: Display<C::Transport>) -> Exit {
        // The device lost whatever it showed: fade the current frame back in
        // (even when paused); the first tick then sends it at full brightness.
        if let Some(p) = &mut self.player {
            p.resume();
        }
        if let Err(e) = self.fade_in(&mut display) {
            self.report(DeviceState::Disconnected, Some(format!("connection lost: {e}")));
            return Exit::Lost;
        }
        let mut due = self.player.as_ref().map(|_| Instant::now());
        loop {
            // `None` means the frame timer fired.
            let cmd = match due {
                Some(d) => match self.rx.recv_timeout(d.saturating_duration_since(Instant::now())) {
                    Ok(cmd) => Some(cmd),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => return Exit::Shutdown,
                },
                None => match self.rx.recv() {
                    Ok(cmd) => Some(cmd),
                    Err(_) => return Exit::Shutdown,
                },
            };

            if let Some(cmd) = cmd {
                match self.apply(cmd) {
                    Effect::SendNow => due = Some(Instant::now()),
                    Effect::Pause => due = None,
                    Effect::Shutdown => {
                        self.fade_out(&mut display);
                        self.backlight_off();
                        return Exit::Shutdown;
                    }
                    Effect::FadeOut(ack) => {
                        self.fade_out(&mut display);
                        self.backlight_off();
                        let _ = ack.send(());
                        due = None; // stay dark until resumed or told otherwise
                    }
                    Effect::Hid(command) => {
                        self.hid_request(command);
                    }
                    Effect::Reconnect(reason) => {
                        info!("reconnecting: {reason}");
                        self.report(DeviceState::Disconnected, None);
                        return Exit::Lost;
                    }
                    Effect::Release(ack) => {
                        self.connector.on_release(display.transport_mut());
                        drop(display);
                        self.hid = None;
                        info!("display released to Windows");
                        self.report(DeviceState::Disconnected, Some("released to Windows (device disabled or removed)".into()));
                        let _ = ack.send(());
                        return Exit::Released;
                    }
                    Effect::Arrived => {}
                }
                continue;
            }

            let Some(player) = self.player.as_mut() else {
                due = None;
                continue;
            };
            match player.tick(&mut display, Instant::now()) {
                Ok(next) => {
                    self.record_frame();
                    // While paused a new source still gets its first frame shown.
                    due = if self.paused { None } else { next };
                }
                Err(PlayError::Display(e)) => {
                    self.report(DeviceState::Disconnected, Some(format!("connection lost: {e}")));
                    return Exit::Lost;
                }
                Err(e @ PlayError::Cache(_)) => {
                    warn!("{e}; stopping playback");
                    lock(&self.shared).last_error = Some(e.to_string());
                    self.player = None;
                    due = None;
                }
            }
        }
    }

    /// Opens HID control, reads the device info, restores the display mode
    /// MSI's software turns off at shutdown, and applies our settings.
    fn setup_hid(&mut self) {
        self.hid = None;
        let mut client = match self.connector.open_hid() {
            Ok(t) => HidClient::new(t),
            Err(e) => {
                warn!("HID control unavailable ({e}); brightness and rotation are not set");
                return;
            }
        };
        let info = match client.request(HidCommand::Conn) {
            Ok(r) => serde_json::from_str::<DeviceInfo>(&r.body).unwrap_or_default(),
            Err(e) => {
                warn!("HID control not answering ({e}); brightness and rotation are not set");
                return;
            }
        };
        info!(firmware = info.version.firmware, app = info.version.app, degree = info.degree, "device info");
        lock(&self.shared).firmware = Some(info.version.firmware.clone()).filter(|f| !f.is_empty());
        self.hid = Some(client);
        if info.extended_display == 0 {
            self.hid_request(HidCommand::ExtendedDisplay(true));
        }
        if let Some(r) = self.settings.rotation.filter(|r| r.degrees() != info.degree) {
            self.hid_request(HidCommand::Rotate(r));
        }
        self.hid_request(HidCommand::Brightness(self.settings.brightness));
    }

    fn backlight_off(&mut self) {
        if self.hid_request(HidCommand::Brightness(0)) {
            debug!("backlight off");
        }
    }

    /// Best effort: on failure HID is dropped until the next connect.
    fn hid_request(&mut self, command: HidCommand) -> bool {
        let Some(client) = self.hid.as_mut() else { return false };
        match client.request(command) {
            Ok(_) => true,
            Err(e) => {
                warn!("HID {} failed: {e}; HID control off until reconnect", command.name());
                self.hid = None;
                false
            }
        }
    }

    /// Fades from black up to the frame playback will show next. Only a USB
    /// error is returned; a frame that can't be faded is just skipped.
    fn fade_in(&mut self, display: &mut Display<C::Transport>) -> Result<(), aio_proto::Error> {
        let Some(player) = self.player.as_mut() else { return Ok(()) };
        let frames = match player.peek_next().map_err(anyhow::Error::from).and_then(|f| render::fade_from_black(f, FADE_STEPS)) {
            Ok(frames) => frames,
            Err(e) => {
                warn!("cannot fade in: {e:#}");
                return Ok(());
            }
        };
        self.send_paced(display, &frames)
    }

    /// Sends `frames` evenly over the fade duration.
    fn send_paced(&self, display: &mut Display<C::Transport>, frames: &[Vec<u8>]) -> Result<(), aio_proto::Error> {
        let step = self.timing.fade / FADE_STEPS;
        let mut due = Instant::now();
        for jpeg in frames {
            display.send_jpeg(jpeg)?;
            due += step;
            std::thread::sleep(due.saturating_duration_since(Instant::now()));
        }
        Ok(())
    }

    /// Fades the frame on screen to black. Best effort: any error just ends it.
    fn fade_out(&mut self, display: &mut Display<C::Transport>) {
        let Some(frame) = self.player.as_ref().map(Player::current_frame).filter(|f| !f.is_empty()) else {
            return; // nothing of ours on screen
        };
        let frames = match render::fade_to_black(frame, FADE_STEPS) {
            Ok(frames) => frames,
            Err(e) => {
                warn!("cannot fade out: {e:#}");
                return;
            }
        };
        if let Err(e) = self.send_paced(display, &frames) {
            debug!("fade interrupted: {e}");
            return;
        }
        lock(&self.shared).preview.clone_from(frames.last().expect("FADE_STEPS > 0"));
        debug!("faded to black");
    }

    fn record_frame(&mut self) {
        let now = Instant::now();
        let mut s = lock(&self.shared);
        if s.last_send.is_none_or(|t| now.duration_since(t) > Duration::from_secs(1)) {
            // Coming out of idle: don't average over the gap.
            self.meter = FpsMeter { window_start: now, frames: 0 };
        }
        s.frames_sent += 1;
        s.last_send = Some(now);
        if let Some(p) = &self.player {
            s.preview.clear();
            s.preview.extend_from_slice(p.current_frame());
        }
        self.meter.frames += 1;
        let elapsed = now.duration_since(self.meter.window_start);
        if elapsed >= Duration::from_secs(1) {
            s.fps = self.meter.frames as f32 / elapsed.as_secs_f32();
            self.meter = FpsMeter { window_start: now, frames: 0 };
        }
    }
}

#[cfg(test)]
#[path = "device_tests.rs"]
mod tests;
