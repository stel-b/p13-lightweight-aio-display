//! Playback: read cached bytes → bulk write → sleep. No decoding at runtime.

use std::io;
use std::time::{Duration, Instant};

use aio_proto::{Display, Transport};

use crate::media::CachedMedia;

/// If a frame goes out later than this past its deadline (system sleep, USB
/// stall), timing restarts from now instead of rushing to catch up.
const MAX_LAG: Duration = Duration::from_millis(250);

#[derive(Debug, thiserror::Error)]
pub enum PlayError {
    #[error("reading cached frame: {0}")]
    Cache(#[from] io::Error),
    #[error(transparent)]
    Display(#[from] aio_proto::Error),
}

pub struct Player {
    media: CachedMedia,
    next: usize,
    /// When the frame at `next` is due; `None` means "now".
    deadline: Option<Instant>,
    /// A static source has been shown; nothing more to send.
    done: bool,
    buf: Vec<u8>,
}

impl Player {
    pub fn new(media: CachedMedia) -> Self {
        Self { media, next: 0, deadline: None, done: false, buf: Vec::new() }
    }

    pub fn media(&self) -> &CachedMedia {
        &self.media
    }

    /// The JPEG most recently sent (empty before the first tick).
    pub fn current_frame(&self) -> &[u8] {
        &self.buf
    }

    /// The JPEG the next tick will send, without sending it.
    pub fn peek_next(&mut self) -> io::Result<&[u8]> {
        self.media.read_frame(self.next, &mut self.buf)?;
        Ok(&self.buf)
    }

    /// Sends the next frame. Returns when the following frame is due, or
    /// `None` once a static source has been shown.
    ///
    /// On error nothing advances, so after reconnecting the same frame is
    /// retried.
    pub fn tick<T: Transport>(
        &mut self,
        display: &mut Display<T>,
        now: Instant,
    ) -> Result<Option<Instant>, PlayError> {
        if self.done {
            return Ok(None);
        }
        self.media.read_frame(self.next, &mut self.buf)?;
        display.send_jpeg(&self.buf)?;

        if self.media.is_static() {
            self.done = true;
            return Ok(None);
        }
        let base = match self.deadline {
            Some(d) if now.saturating_duration_since(d) <= MAX_LAG => d,
            _ => now,
        };
        let due = base + Duration::from_micros(self.media.frames()[self.next].duration_us);
        self.next = (self.next + 1) % self.media.frames().len();
        self.deadline = Some(due);
        Ok(Some(due))
    }

    /// Call after a reconnect: the current frame is sent again right away
    /// (static sources included).
    pub fn resume(&mut self) {
        self.done = false;
        self.deadline = None;
    }

    /// Ticks until a static source is shown; loops forever for animations.
    pub fn run<T: Transport>(&mut self, display: &mut Display<T>) -> Result<(), PlayError> {
        while let Some(due) = self.tick(display, Instant::now())? {
            std::thread::sleep(due.saturating_duration_since(Instant::now()));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::{self, Source};
    use aio_proto::mock::{MockDevice, test_handshake};
    use image::codecs::gif::GifEncoder;
    use image::{Delay, Frame, Rgba, RgbaImage};

    fn display() -> Display<MockDevice> {
        Display::connect(MockDevice::new(test_handshake()), &aio_proto::Auth::Replay(test_handshake())).unwrap()
    }

    fn gif_player(dir: &std::path::Path, delays: &[u32]) -> Player {
        let path = dir.join("a.gif");
        let mut enc = GifEncoder::new(std::fs::File::create(&path).unwrap());
        for (i, &ms) in delays.iter().enumerate() {
            let img = RgbaImage::from_pixel(8, 8, Rgba([i as u8 * 60, 0, 0, 255]));
            enc.encode_frame(Frame::from_parts(img, 0, 0, Delay::from_numer_denom_ms(ms, 1)))
                .unwrap();
        }
        drop(enc);
        Player::new(media::import(&Source::Gif { path }, &dir.join("cache")).unwrap())
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn static_source_is_sent_once() {
        let tmp = tempfile::tempdir().unwrap();
        let media = media::import(&Source::Color { rgb: [255, 0, 0] }, tmp.path()).unwrap();
        let mut player = Player::new(media);
        let mut d = display();
        let t0 = Instant::now();
        assert_eq!(player.tick(&mut d, t0).unwrap(), None);
        assert_eq!(player.tick(&mut d, t0).unwrap(), None);
        assert_eq!(d.transport().frames.len(), 1);

        player.resume();
        assert_eq!(player.tick(&mut d, t0).unwrap(), None);
        assert_eq!(d.transport().frames.len(), 2);
    }

    #[test]
    fn animation_loops_with_drift_free_deadlines() {
        let tmp = tempfile::tempdir().unwrap();
        let mut player = gif_player(tmp.path(), &[50, 100, 30]);
        let mut d = display();
        let t0 = Instant::now();

        let due1 = player.tick(&mut d, t0).unwrap().unwrap();
        assert_eq!(due1, t0 + ms(50));
        // Woke 4 ms late: the next deadline still counts from the planned time.
        let due2 = player.tick(&mut d, due1 + ms(4)).unwrap().unwrap();
        assert_eq!(due2, t0 + ms(150));
        let due3 = player.tick(&mut d, due2).unwrap().unwrap();
        assert_eq!(due3, t0 + ms(180));
        let due4 = player.tick(&mut d, due3).unwrap().unwrap();
        assert_eq!(due4, t0 + ms(230), "wrapped to the first frame's delay");

        let frames = &d.transport().frames;
        assert_eq!(frames.len(), 4);
        assert_eq!(frames[0].1, frames[3].1, "loop restarted at frame 0");
        assert_ne!(frames[0].1, frames[1].1);
    }

    #[test]
    fn long_stall_resets_timing() {
        let tmp = tempfile::tempdir().unwrap();
        let mut player = gif_player(tmp.path(), &[50, 50]);
        let mut d = display();
        let t0 = Instant::now();
        let due = player.tick(&mut d, t0).unwrap().unwrap();
        let late = due + Duration::from_secs(5);
        assert_eq!(player.tick(&mut d, late).unwrap().unwrap(), late + ms(50));
    }

    #[test]
    fn failed_send_retries_same_frame() {
        let tmp = tempfile::tempdir().unwrap();
        let mut player = gif_player(tmp.path(), &[50, 50]);
        let mut d = display();
        let t0 = Instant::now();
        player.tick(&mut d, t0).unwrap(); // frame 0

        d.transport_mut().unplugged = true;
        let err = player.tick(&mut d, t0 + ms(50)).unwrap_err(); // frame 1 fails
        assert!(matches!(err, PlayError::Display(_)));

        // After reconnecting, frame 1 goes out immediately.
        let mut reconnected = display();
        player.resume();
        let t1 = t0 + Duration::from_secs(1);
        assert_eq!(player.tick(&mut reconnected, t1).unwrap(), Some(t1 + ms(50)));
        let frame0 = &d.transport().frames[0].1;
        let resent = &reconnected.transport().frames[0].1;
        assert_ne!(frame0, resent, "should resend frame 1, not restart at 0");
    }
}
