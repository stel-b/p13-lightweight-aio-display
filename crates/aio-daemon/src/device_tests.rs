use super::*;
use crate::media::{self, Source};
use aio_proto::hid::Rotation;
use aio_proto::mock::{MockDevice, MockHid, mock_device_key, test_handshake};
use aio_proto::{TransportError, VendorRequest};
use image::codecs::gif::GifEncoder;
use image::{Delay, Frame, Rgba, RgbaImage};
use std::collections::VecDeque;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Sender};
use std::thread::JoinHandle;

/// A mock device the test can still reach after the worker took it.
#[derive(Clone)]
struct SharedMock(Arc<Mutex<MockDevice>>);

impl SharedMock {
    fn get(&self) -> MutexGuard<'_, MockDevice> {
        self.0.lock().unwrap()
    }
}

impl Transport for SharedMock {
    fn control_in(&mut self, r: VendorRequest, l: u16, t: Duration) -> Result<Vec<u8>, TransportError> {
        self.get().control_in(r, l, t)
    }
    fn control_out(&mut self, r: VendorRequest, d: &[u8], t: Duration) -> Result<(), TransportError> {
        self.get().control_out(r, d, t)
    }
    fn bulk_write(&mut self, d: &[u8], t: Duration) -> Result<(), TransportError> {
        self.get().bulk_write(d, t)
    }
    fn bulk_read(&mut self, m: usize, t: Duration) -> Result<Vec<u8>, TransportError> {
        self.get().bulk_read(m, t)
    }
    fn sleep(&mut self, _: Duration) {}
}

/// A mock HID interface the test can still reach after the worker took it.
#[derive(Clone)]
struct SharedHid(Arc<Mutex<MockHid>>);

impl SharedHid {
    fn get(&self) -> MutexGuard<'_, MockHid> {
        self.0.lock().unwrap()
    }
}

impl aio_proto::hid::HidTransport for SharedHid {
    fn write_report(&mut self, r: &[u8; aio_proto::hid::REPORT_LEN]) -> Result<(), TransportError> {
        self.get().write_report(r)
    }
    fn read_report(&mut self, t: Duration) -> Result<Vec<u8>, TransportError> {
        self.get().read_report(t)
    }
    fn flush_input(&mut self) -> Result<(), TransportError> {
        self.get().flush_input()
    }
}

/// Hands out a fresh mock per connection; scripted failures come first.
#[derive(Clone, Default)]
struct TestConnector {
    failures: Arc<Mutex<VecDeque<OpenError>>>,
    devices: Arc<Mutex<Vec<SharedMock>>>,
    releases: Arc<AtomicUsize>,
    /// Offer the computed handshake (which the replay-mode mock can't answer)
    /// until told it failed.
    computed_first: Arc<std::sync::atomic::AtomicBool>,
    auth_failures: Arc<AtomicUsize>,
    /// HID interfaces handed out, one per connection.
    hids: Arc<Mutex<Vec<SharedHid>>>,
    /// The next HID interface starts with extendedDisplay off.
    hid_extended_off: Arc<std::sync::atomic::AtomicBool>,
    /// Opening the HID interface fails.
    no_hid: Arc<std::sync::atomic::AtomicBool>,
}

impl TestConnector {
    fn device(&self, i: usize) -> SharedMock {
        self.devices.lock().unwrap()[i].clone()
    }
    fn connections(&self) -> usize {
        self.devices.lock().unwrap().len()
    }
    /// Requests seen by the `i`-th HID interface (empty if none yet).
    fn hid_requests(&self, i: usize) -> Vec<String> {
        let hid = self.hids.lock().unwrap().get(i).cloned();
        hid.map_or_else(Vec::new, |h| h.get().requests.clone())
    }
}

impl Connector for TestConnector {
    type Transport = SharedMock;
    type Hid = SharedHid;
    fn open_hid(&mut self) -> Result<SharedHid, OpenError> {
        if self.no_hid.load(Ordering::SeqCst) {
            return Err(OpenError::NotFound);
        }
        let mut mock = MockHid::default();
        mock.extended_display = !self.hid_extended_off.load(Ordering::SeqCst);
        let hid = SharedHid(Arc::new(Mutex::new(mock)));
        self.hids.lock().unwrap().push(hid.clone());
        Ok(hid)
    }
    fn auth(&mut self) -> anyhow::Result<Auth> {
        if self.computed_first.load(Ordering::SeqCst) {
            return Ok(Auth::Computed(mock_device_key()));
        }
        Ok(Auth::Replay(test_handshake()))
    }
    fn on_auth_failed(&mut self, auth: &Auth) {
        assert!(matches!(auth, Auth::Computed(_)));
        self.auth_failures.fetch_add(1, Ordering::SeqCst);
        self.computed_first.store(false, Ordering::SeqCst);
    }
    fn open(&mut self) -> Result<SharedMock, OpenError> {
        if let Some(e) = self.failures.lock().unwrap().pop_front() {
            return Err(e);
        }
        let dev = SharedMock(Arc::new(Mutex::new(MockDevice::new(test_handshake()))));
        self.devices.lock().unwrap().push(dev.clone());
        Ok(dev)
    }
    fn on_release(&mut self, _: &mut SharedMock) {
        self.releases.fetch_add(1, Ordering::SeqCst);
    }
}

struct Harness {
    tx: Sender<Command>,
    shared: Arc<Mutex<Shared>>,
    connector: TestConnector,
    thread: Option<JoinHandle<()>>,
    cache: PathBuf,
    _tmp: tempfile::TempDir,
}

impl Harness {
    fn start(connector: TestConnector, initial: Option<Source>, paused: bool) -> Self {
        Self::start_with_hold(connector, initial, paused, Duration::from_secs(10))
    }

    fn start_with_hold(connector: TestConnector, initial: Option<Source>, paused: bool, hold: Duration) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().join("cache");
        let media = initial.map(|s| media::import(&s, &cache).unwrap());
        let (tx, rx) = mpsc::channel();
        let shared = Arc::new(Mutex::new(Shared::new(paused)));
        let timing = Timing {
            min: Duration::from_millis(5),
            max: Duration::from_millis(20),
            release_hold: hold,
            fade: Duration::from_millis(48),
        };
        let worker =
            Worker::new(connector.clone(), rx, shared.clone(), media, paused, DeviceSettings::default(), timing);
        let thread = Some(std::thread::spawn(move || worker.run()));
        Self { tx, shared, connector, thread, cache, _tmp: tmp }
    }

    fn set(&self, source: Source) {
        self.tx.send(Command::SetMedia(media::import(&source, &self.cache).unwrap())).unwrap();
    }

    fn gif(&self, delays_ms: &[u32]) -> Source {
        let path = self.cache.parent().unwrap().join(format!("{}.gif", delays_ms.len()));
        write_gif(&path, delays_ms);
        Source::Gif { path }
    }

    /// Frames received by the `device`-th connection (0 if not connected yet).
    fn frames(&self, device: usize) -> usize {
        let dev = self.connector.devices.lock().unwrap().get(device).cloned();
        dev.map_or(0, |d| d.get().frames.len())
    }

    fn state(&self) -> DeviceState {
        lock(&self.shared).device
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = self.tx.send(Command::Shutdown);
        if let Some(t) = self.thread.take() {
            let joined = t.join();
            if !std::thread::panicking() {
                joined.expect("worker thread panicked");
            }
        }
    }
}

fn write_gif(path: &Path, delays_ms: &[u32]) {
    let mut enc = GifEncoder::new(std::fs::File::create(path).unwrap());
    for (i, &ms) in delays_ms.iter().enumerate() {
        let img = RgbaImage::from_pixel(8, 8, Rgba([0, i as u8 * 50, 0, 255]));
        enc.encode_frame(Frame::from_parts(img, 0, 0, Delay::from_numer_denom_ms(ms, 1))).unwrap();
    }
}

fn eventually(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for: {what}");
        std::thread::sleep(Duration::from_millis(2));
    }
}

const RED: Source = Source::Color { rgb: [255, 0, 0] };
/// Frames a connection gets for a source that is already set: the fade-in,
/// then the frame itself.
const SHOWN: usize = FADE_STEPS as usize + 1;
const BLUE: Source = Source::Color { rgb: [0, 0, 255] };

#[test]
fn restores_static_source_once() {
    let h = Harness::start(TestConnector::default(), Some(RED), false);
    eventually("first frame", || h.connector.connections() == 1 && h.frames(0) == SHOWN);
    assert_eq!(h.state(), DeviceState::Connected);
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(h.frames(0), SHOWN, "static source must not be resent");
    assert_eq!(lock(&h.shared).frames_sent, 1);
    assert!(lock(&h.shared).preview.starts_with(&[0xFF, 0xD8]));
}

#[test]
fn connects_without_source_and_switches_live() {
    let h = Harness::start(TestConnector::default(), None, false);
    eventually("connected", || h.state() == DeviceState::Connected);
    assert_eq!(h.frames(0), 0);

    h.set(RED);
    eventually("red", || h.frames(0) == 1);
    h.set(BLUE);
    eventually("blue", || h.frames(0) == 2);
    let dev = h.connector.device(0);
    let dev = dev.get();
    assert_ne!(dev.frames[0].1, dev.frames[1].1);
}

#[test]
fn animation_reconnects_after_usb_error() {
    let h = Harness::start(TestConnector::default(), None, false);
    h.set(h.gif(&[20, 20]));
    eventually("animation running", || h.frames(0) >= 3);

    h.connector.device(0).get().unplugged = true;
    eventually("reconnected", || h.connector.connections() == 2);
    eventually("animation resumed on new connection", || h.frames(1) >= 3);
    assert_eq!(h.state(), DeviceState::Connected);
    assert!(lock(&h.shared).last_error.is_none());
}

#[test]
fn static_source_is_resent_after_replug() {
    let h = Harness::start(TestConnector::default(), Some(RED), false);
    eventually("first frame", || h.frames(0) == SHOWN);

    h.connector.device(0).get().unplugged = true;
    h.tx.send(Command::Reconnect("removed")).unwrap();
    h.tx.send(Command::DeviceArrived).unwrap();
    eventually("resent after replug", || h.connector.connections() == 2 && h.frames(1) == SHOWN);
}

#[test]
fn retries_while_busy() {
    let connector = TestConnector::default();
    connector.failures.lock().unwrap().extend([
        OpenError::Busy("access denied".into()),
        OpenError::Busy("access denied".into()),
        OpenError::NotFound,
    ]);
    let h = Harness::start(connector, Some(RED), false);
    eventually("connected after failures", || h.connector.connections() == 1 && h.frames(0) == SHOWN);
    assert_eq!(h.state(), DeviceState::Connected);
}

#[test]
fn busy_state_is_reported() {
    let connector = TestConnector::default();
    connector.failures.lock().unwrap().extend((0..1000).map(|_| OpenError::Busy("steam".into())));
    let h = Harness::start(connector, Some(RED), false);
    eventually("busy", || h.state() == DeviceState::Busy);
    assert!(lock(&h.shared).last_error.as_deref().unwrap().contains("steam"));
}

#[test]
fn pause_and_resume() {
    let h = Harness::start(TestConnector::default(), None, false);
    h.set(h.gif(&[20, 20]));
    eventually("running", || h.frames(0) >= 2);

    h.tx.send(Command::Pause).unwrap();
    eventually("paused flag", || lock(&h.shared).paused);
    std::thread::sleep(Duration::from_millis(30)); // let an in-flight tick finish
    let frozen = h.frames(0);
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(h.frames(0), frozen, "frames sent while paused");

    h.tx.send(Command::Resume).unwrap();
    eventually("running again", || h.frames(0) >= frozen + 3);
}

#[test]
fn paused_new_source_shows_first_frame_only() {
    let h = Harness::start(TestConnector::default(), None, true);
    eventually("connected", || h.state() == DeviceState::Connected);
    h.set(h.gif(&[20, 20, 20]));
    eventually("first frame", || h.frames(0) == 1);
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(h.frames(0), 1);
}

#[test]
fn shutdown_while_disconnected() {
    let connector = TestConnector::default();
    connector.failures.lock().unwrap().extend((0..1000).map(|_| OpenError::NotFound));
    let h = Harness::start(connector, None, false);
    eventually("tried", || lock(&h.shared).last_error.is_some());
    drop(h); // joins the thread; hangs if shutdown is ignored
}

fn release(h: &Harness) {
    let (ack_tx, ack_rx) = mpsc::sync_channel(1);
    h.tx.send(Command::Release(ack_tx)).unwrap();
    ack_rx.recv_timeout(Duration::from_secs(2)).expect("release was not acknowledged");
}

#[test]
fn releases_device_and_waits_for_it_to_return() {
    let h = Harness::start(TestConnector::default(), Some(RED), false);
    eventually("first frame", || h.frames(0) == SHOWN);

    release(&h);
    assert_eq!(h.connector.releases.load(Ordering::SeqCst), 1);
    // The transport is gone: only the connector's list and our clone remain.
    assert_eq!(Arc::strong_count(&h.connector.device(0).0), 2);
    assert_eq!(h.state(), DeviceState::Disconnected);

    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(h.connector.connections(), 1, "reopened during the release hold");

    h.tx.send(Command::DeviceArrived).unwrap(); // re-enabled
    eventually("reconnected and resent", || h.connector.connections() == 2 && h.frames(1) == SHOWN);
}

#[test]
fn reconnects_after_release_hold_without_arrival() {
    let h = Harness::start_with_hold(TestConnector::default(), Some(RED), false, Duration::from_millis(50));
    eventually("first frame", || h.frames(0) == SHOWN);
    release(&h);
    eventually("reconnected after hold", || h.connector.connections() == 2 && h.frames(1) == SHOWN);
}

#[test]
fn release_while_disconnected_is_acknowledged() {
    let connector = TestConnector::default();
    connector.failures.lock().unwrap().extend((0..1000).map(|_| OpenError::NotFound));
    let h = Harness::start(connector, None, false);
    eventually("tried", || lock(&h.shared).last_error.is_some());
    release(&h);
    assert_eq!(h.connector.releases.load(Ordering::SeqCst), 0);
}

/// Red channel at the center of frame `i` received by connection `dev`.
fn red_at(connector: &TestConnector, dev: usize, i: usize) -> u8 {
    let jpeg = connector.device(dev).get().frames[i].1.clone();
    image::load_from_memory(&jpeg).unwrap().to_rgb8().get_pixel(240, 240).0[0]
}

#[test]
fn shutdown_fades_to_black() {
    let h = Harness::start(TestConnector::default(), Some(RED), false);
    eventually("first frame", || h.frames(0) == SHOWN);
    let connector = h.connector.clone();
    drop(h); // sends Shutdown and joins

    let sent = connector.device(0).get().frames.len();
    assert_eq!(sent, SHOWN + FADE_STEPS as usize);
    assert!(red_at(&connector, 0, SHOWN) > 200, "fade starts near full brightness");
    assert!(red_at(&connector, 0, sent - 1) <= 2, "fade ends black");
}

#[test]
fn shutdown_without_source_sends_nothing() {
    let h = Harness::start(TestConnector::default(), None, false);
    eventually("connected", || h.state() == DeviceState::Connected);
    let connector = h.connector.clone();
    drop(h);
    assert_eq!(connector.device(0).get().frames.len(), 0);
}

#[test]
fn sleep_fades_out_and_resume_restores() {
    let h = Harness::start(TestConnector::default(), None, false);
    h.set(h.gif(&[20, 20]));
    eventually("running", || h.frames(0) >= 2);

    let (ack_tx, ack_rx) = mpsc::sync_channel(1);
    h.tx.send(Command::FadeOut(ack_tx)).unwrap();
    ack_rx.recv_timeout(Duration::from_secs(2)).expect("fade not acknowledged");
    let after_fade = h.frames(0);
    assert!(red_at(&h.connector, 0, after_fade - 1) <= 2, "screen not black after fade");
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(h.frames(0), after_fade, "animation kept running after the fade");

    h.tx.send(Command::Reconnect("system resumed")).unwrap();
    eventually("playing again after resume", || h.frames(1) >= 3);
}

#[test]
fn connecting_fades_in_from_black() {
    let h = Harness::start(TestConnector::default(), Some(RED), false);
    eventually("shown", || h.frames(0) == SHOWN);
    assert!(red_at(&h.connector, 0, 0) <= 2, "fade-in starts black");
    let reds: Vec<u8> = (0..SHOWN).map(|i| red_at(&h.connector, 0, i)).collect();
    assert!(reds.windows(2).all(|w| w[0] <= w[1]), "not brightening: {reds:?}");
    assert!(reds[SHOWN - 1] > 245, "ends at full brightness: {reds:?}");
    assert_eq!(lock(&h.shared).frames_sent, 1, "fade frames are not playback frames");
}

#[test]
fn falls_back_after_auth_failure() {
    let connector = TestConnector::default();
    connector.computed_first.store(true, Ordering::SeqCst);
    let h = Harness::start(connector, Some(RED), false);
    eventually("connected via replay", || h.state() == DeviceState::Connected);
    assert_eq!(h.connector.auth_failures.load(Ordering::SeqCst), 1);
    assert_eq!(h.connector.connections(), 2, "one failed computed attempt, one replay");
}

const FULL: &str = "brightness {\"value\":100}";
const OFF: &str = "brightness {\"value\":0}";

#[test]
fn connect_reads_info_then_sets_brightness() {
    let h = Harness::start(TestConnector::default(), Some(RED), false);
    eventually("shown", || h.frames(0) == SHOWN);
    assert_eq!(h.connector.hid_requests(0), ["conn", FULL]);
    assert_eq!(lock(&h.shared).firmware.as_deref(), Some("P13_20251204v01"));
}

#[test]
fn restores_extended_display_like_msi() {
    let connector = TestConnector::default();
    connector.hid_extended_off.store(true, Ordering::SeqCst);
    let h = Harness::start(connector, None, false);
    eventually("connected", || h.state() == DeviceState::Connected && h.connector.hid_requests(0).len() == 3);
    assert_eq!(h.connector.hid_requests(0), ["conn", "extendedDisplay {\"enable\":true}", FULL]);
}

#[test]
fn shutdown_turns_backlight_off_after_fade() {
    let h = Harness::start(TestConnector::default(), Some(RED), false);
    eventually("shown", || h.frames(0) == SHOWN);
    let connector = h.connector.clone();
    drop(h);
    assert_eq!(connector.device(0).get().frames.len(), SHOWN + FADE_STEPS as usize, "faded first");
    assert_eq!(connector.hid_requests(0).last().map(String::as_str), Some(OFF));
}

#[test]
fn sleep_turns_backlight_off_and_resume_restores_it() {
    let h = Harness::start(TestConnector::default(), Some(RED), false);
    eventually("shown", || h.frames(0) == SHOWN);
    let (ack_tx, ack_rx) = mpsc::sync_channel(1);
    h.tx.send(Command::FadeOut(ack_tx)).unwrap();
    ack_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(h.connector.hid_requests(0).last().map(String::as_str), Some(OFF));

    h.tx.send(Command::Reconnect("system resumed")).unwrap();
    eventually("reconnected", || h.connector.hid_requests(1).len() == 2);
    assert_eq!(h.connector.hid_requests(1), ["conn", FULL]);
}

#[test]
fn settings_apply_live_and_on_next_connect() {
    let h = Harness::start(TestConnector::default(), Some(RED), false);
    eventually("shown", || h.frames(0) == SHOWN);
    h.tx.send(Command::SetBrightness(30)).unwrap();
    h.tx.send(Command::SetRotation(Rotation::Deg90)).unwrap();
    eventually("applied", || h.connector.hid_requests(0).len() == 4);
    assert_eq!(
        h.connector.hid_requests(0),
        ["conn", FULL, "brightness {\"value\":30}", "rotate {\"degree\":90}"],
        "only the changed setting is sent"
    );

    h.tx.send(Command::Reconnect("test")).unwrap();
    eventually("reconnected", || h.connector.hid_requests(1).len() == 3);
    assert_eq!(h.connector.hid_requests(1), ["conn", "rotate {\"degree\":90}", "brightness {\"value\":30}"]);
}

#[test]
fn display_works_without_hid() {
    let connector = TestConnector::default();
    connector.no_hid.store(true, Ordering::SeqCst);
    let h = Harness::start(connector, Some(RED), false);
    eventually("shown", || h.frames(0) == SHOWN);
    h.tx.send(Command::SetBrightness(10)).unwrap(); // must not panic or disconnect
    std::thread::sleep(Duration::from_millis(30));
    assert_eq!(h.state(), DeviceState::Connected);
    assert!(lock(&h.shared).firmware.is_none());
}
