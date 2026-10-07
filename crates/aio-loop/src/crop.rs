//! A square crop of the video, in source pixels (after rotation metadata is
//! applied, the same space ffmpeg's `crop` filter works in).

/// Smallest crop side; anything smaller is useless on a 480 px display.
pub const MIN_SIZE: u32 = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Crop {
    pub x: u32,
    pub y: u32,
    pub size: u32,
}

/// Even values keep 4:2:0 video happy.
fn even(v: f64) -> u32 {
    (v.max(0.0) as u32) & !1
}

impl Crop {
    /// The largest centered square: what the daemon does by default.
    pub fn centered_max(width: u32, height: u32) -> Self {
        let size = width.min(height) & !1;
        Self { x: even(f64::from(width - size) / 2.0), y: even(f64::from(height - size) / 2.0), size }
    }

    /// A square of `size` centered on (`cx`, `cy`), clamped to the frame.
    pub fn around(cx: f64, cy: f64, size: f64, width: u32, height: u32) -> Self {
        let max = width.min(height) & !1;
        let size = even(size).clamp(MIN_SIZE.min(max), max);
        let half = f64::from(size) / 2.0;
        let x = even(cx - half).min(width - size);
        let y = even(cy - half).min(height - size);
        Self { x, y, size }
    }

    pub fn center(&self) -> (f64, f64) {
        let half = f64::from(self.size) / 2.0;
        (f64::from(self.x) + half, f64::from(self.y) + half)
    }

    /// The ffmpeg filter for this crop.
    pub fn filter(&self) -> String {
        format!("crop={s}:{s}:{x}:{y}", s = self.size, x = self.x, y = self.y)
    }

    /// Parses `x:y:size`.
    pub fn parse(text: &str) -> Option<Self> {
        let mut parts = text.split(':').map(|p| p.trim().parse::<u32>().ok());
        let (x, y, size) = (parts.next()??, parts.next()??, parts.next()??);
        (parts.next().is_none() && size > 0).then_some(Self { x, y, size })
    }

    pub fn fits(&self, width: u32, height: u32) -> bool {
        self.size > 0 && self.x + self.size <= width && self.y + self.size <= height
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn centered_max_matches_the_daemon() {
        assert_eq!(Crop::centered_max(1920, 1080), Crop { x: 420, y: 0, size: 1080 });
        assert_eq!(Crop::centered_max(1080, 1920), Crop { x: 0, y: 420, size: 1080 });
        assert_eq!(Crop::centered_max(480, 480), Crop { x: 0, y: 0, size: 480 });
    }

    #[test]
    fn around_clamps_and_stays_even() {
        // Pushed past the right edge: slides back inside.
        let c = Crop::around(1900.0, 540.0, 501.0, 1920, 1080);
        assert_eq!(c.size, 500);
        assert_eq!(c.x + c.size, 1920);
        assert!(c.x.is_multiple_of(2) && c.y.is_multiple_of(2));
        // Too big or too small: limited to the frame / the minimum.
        assert_eq!(Crop::around(960.0, 540.0, 5000.0, 1920, 1080).size, 1080);
        assert_eq!(Crop::around(960.0, 540.0, 1.0, 1920, 1080).size, MIN_SIZE);
        assert!(Crop::around(-50.0, -50.0, 200.0, 1920, 1080).fits(1920, 1080));
    }

    #[test]
    fn filter_and_parse_roundtrip() {
        let c = Crop { x: 420, y: 10, size: 900 };
        assert_eq!(c.filter(), "crop=900:900:420:10");
        assert_eq!(Crop::parse("420:10:900"), Some(c));
        assert_eq!(Crop::parse("1:2"), None);
        assert_eq!(Crop::parse("1:2:0"), None);
        assert!(!c.fits(1280, 720));
    }
}
