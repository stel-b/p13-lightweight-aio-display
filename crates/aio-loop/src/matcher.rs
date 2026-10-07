//! Scores how well each (start, end) frame pair closes a loop.
//!
//! A loop `[start, end)` is seamless when frame `end` (the first frame
//! after the loop) looks like frame `start`, and the frames around them
//! match too, so the motion continues across the jump. The score of a pair
//! is the mean difference of `start + k` vs `end + k` for `k` in
//! `-CONTEXT..=CONTEXT`, as a fraction of full scale (0 = identical).

/// Frames compared on each side of the cut point.
pub const CONTEXT: i64 = 2;
/// Side of the tiny RGB thumbnail frames are compared on.
pub const THUMB: usize = 32;

/// A frame reduced to a `THUMB`×`THUMB` RGB thumbnail.
#[derive(Clone)]
pub struct Thumb(Vec<u8>);

impl Thumb {
    /// Box-filters packed RGB (`width`×`height`) down to the thumbnail.
    pub fn from_rgb(rgb: &[u8], width: usize, height: usize) -> Self {
        let mut out = vec![0u8; THUMB * THUMB * 3];
        for ty in 0..THUMB {
            let (y0, y1) = (ty * height / THUMB, ((ty + 1) * height / THUMB).max(ty * height / THUMB + 1));
            for tx in 0..THUMB {
                let (x0, x1) = (tx * width / THUMB, ((tx + 1) * width / THUMB).max(tx * width / THUMB + 1));
                let mut sum = [0u32; 3];
                for y in y0..y1.min(height) {
                    for x in x0..x1.min(width) {
                        let i = (y * width + x) * 3;
                        for c in 0..3 {
                            sum[c] += u32::from(rgb[i + c]);
                        }
                    }
                }
                let n = ((y1.min(height) - y0) * (x1.min(width) - x0)).max(1) as u32;
                for c in 0..3 {
                    out[(ty * THUMB + tx) * 3 + c] = (sum[c] / n) as u8;
                }
            }
        }
        Thumb(out)
    }

    /// Mean absolute difference, 0..=1.
    pub fn distance(&self, other: &Thumb) -> f32 {
        let sum: u32 = self.0.iter().zip(&other.0).map(|(&a, &b)| u32::from(a.abs_diff(b))).sum();
        sum as f32 / (self.0.len() as f32 * 255.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Candidate {
    /// First frame of the loop.
    pub start: u64,
    /// First frame after the loop (it matches `start` and is not included).
    pub end: u64,
    /// 0 = perfect match.
    pub score: f32,
}

impl Candidate {
    pub fn frames(&self) -> u64 {
        self.end - self.start
    }
}

/// Finds the best loops. `starts[i]` is frame `start0 + i`, `ends[j]` is
/// frame `end0 + j`. Loops shorter than `min_len` frames are skipped.
/// Returns up to `count` candidates, best first, at least `spread` frames
/// apart from each other (so they are real alternatives, not neighbours).
pub fn find(starts: &[Thumb], start0: u64, ends: &[Thumb], end0: u64, min_len: u64, count: usize, spread: u64) -> Vec<Candidate> {
    let (ns, ne) = (starts.len(), ends.len());
    if ns == 0 || ne == 0 {
        return Vec::new();
    }
    // All pairwise distances once (in parallel); scores then read diagonals.
    let mut dist = vec![0f32; ns * ne];
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get()).min(ns);
    let rows_per = ns.div_ceil(threads);
    std::thread::scope(|scope| {
        for (chunk_index, chunk) in dist.chunks_mut(rows_per * ne).enumerate() {
            scope.spawn(move || {
                for (r, row) in chunk.chunks_mut(ne).enumerate() {
                    let a = &starts[chunk_index * rows_per + r];
                    for (j, d) in row.iter_mut().enumerate() {
                        *d = a.distance(&ends[j]);
                    }
                }
            });
        }
    });

    let mut all = Vec::new();
    for i in 0..ns {
        for j in 0..ne {
            let (start, end) = (start0 + i as u64, end0 + j as u64);
            if end < start + min_len.max(1) {
                continue;
            }
            let (mut sum, mut n) = (0f32, 0u32);
            for k in -CONTEXT..=CONTEXT {
                let (ii, jj) = (i as i64 + k, j as i64 + k);
                if (0..ns as i64).contains(&ii) && (0..ne as i64).contains(&jj) {
                    sum += dist[ii as usize * ne + jj as usize];
                    n += 1;
                }
            }
            all.push(Candidate { start, end, score: sum / n as f32 });
        }
    }
    all.sort_by(|a, b| a.score.total_cmp(&b.score));

    let mut picked: Vec<Candidate> = Vec::new();
    for c in all {
        let distinct = picked.iter().all(|p| p.start.abs_diff(c.start) >= spread || p.end.abs_diff(c.end) >= spread);
        if distinct {
            picked.push(c);
            if picked.len() == count {
                break;
            }
        }
    }
    picked
}

/// The score of one (start, end) pair from two short windows of thumbnails
/// around them: `a[a_center]` and `b[b_center]` are the two frames, and up
/// to `CONTEXT` neighbours on each side are compared too, as in [`find`].
/// `None` if nothing overlaps.
pub fn aligned_score(a: &[Thumb], a_center: usize, b: &[Thumb], b_center: usize) -> Option<f32> {
    let (mut sum, mut n) = (0f32, 0u32);
    for k in -CONTEXT..=CONTEXT {
        let (i, j) = (a_center as i64 + k, b_center as i64 + k);
        if (0..a.len() as i64).contains(&i) && (0..b.len() as i64).contains(&j) {
            sum += a[i as usize].distance(&b[j as usize]);
            n += 1;
        }
    }
    (n > 0).then(|| sum / n as f32)
}

/// A human label for a score.
pub fn quality(score: f32) -> &'static str {
    match score {
        s if s < 0.01 => "excellent",
        s if s < 0.025 => "good",
        s if s < 0.05 => "fair",
        _ => "poor",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A frame of a periodic animation: a bright square moving along a
    /// circle with `period` frames per turn.
    fn frame(n: u64, period: u64) -> Thumb {
        let mut rgb = vec![10u8; 64 * 64 * 3];
        let angle = n as f64 / period as f64 * std::f64::consts::TAU;
        let (cx, cy) = (32.0 + 20.0 * angle.cos(), 32.0 + 20.0 * angle.sin());
        for y in 0..64 {
            for x in 0..64 {
                if (x as f64 - cx).abs() < 6.0 && (y as f64 - cy).abs() < 6.0 {
                    let i = (y * 64 + x) * 3;
                    rgb[i..i + 3].copy_from_slice(&[250, 200, 50]);
                }
            }
        }
        Thumb::from_rgb(&rgb, 64, 64)
    }

    fn window(first: u64, len: u64, period: u64) -> Vec<Thumb> {
        (first..first + len).map(|n| frame(n, period)).collect()
    }

    #[test]
    fn finds_exact_period() {
        // Period 45 frames; search 60..120 for the start and 180..240 for the end.
        let c = find(&window(60, 60, 45), 60, &window(180, 60, 45), 180, 30, 5, 5);
        let best = c[0];
        assert!(best.score < 1e-6, "{best:?}");
        assert_eq!(best.frames() % 45, 0, "{best:?}");
    }

    #[test]
    fn candidates_are_distinct_and_sorted() {
        let c = find(&window(0, 40, 30), 0, &window(100, 40, 30), 100, 10, 5, 4);
        assert!(c.len() > 1);
        assert!(c.windows(2).all(|w| w[0].score <= w[1].score));
        for (i, a) in c.iter().enumerate() {
            for b in &c[i + 1..] {
                assert!(a.start.abs_diff(b.start) >= 4 || a.end.abs_diff(b.end) >= 4);
            }
        }
    }

    #[test]
    fn respects_minimum_length() {
        let c = find(&window(0, 50, 10), 0, &window(20, 50, 10), 20, 60, 3, 1);
        assert!(c.iter().all(|c| c.frames() >= 60), "{c:?}");
    }

    #[test]
    fn motion_direction_matters() {
        // Two frames that look identical but sit in opposite motion: the
        // context comparison must prefer the pair whose neighbours match too.
        let fwd = window(0, 20, 40);
        let rev: Vec<Thumb> = (0..20u64).map(|n| frame(40 - n, 40)).collect();
        let same_dir = find(&fwd, 0, &window(40, 20, 40), 40, 1, 1, 1)[0].score;
        let opposite = find(&fwd, 0, &rev, 40, 1, 1, 1)[0].score;
        assert!(same_dir < opposite, "{same_dir} vs {opposite}");
    }

    #[test]
    fn aligned_score_matches_find() {
        let starts = window(58, 5, 45);
        let ends = window(103, 5, 45); // frame 105 = frame 60 + one period
        let matching = aligned_score(&starts, 2, &ends, 2).unwrap();
        let shifted = aligned_score(&starts, 2, &ends, 0).unwrap();
        // One period apart is identical up to the test pattern's rounding.
        assert!(matching < 0.002 && shifted > matching * 10.0, "{matching} vs {shifted}");
        // Clipped windows (at the start of a video) still compare.
        let clipped = aligned_score(&starts[2..], 0, &ends[2..], 0).unwrap();
        assert!(clipped < 0.002 && shifted > clipped * 10.0, "{clipped} vs {shifted}");
        assert_eq!(aligned_score(&[], 0, &ends, 0), None);
    }

    #[test]
    fn thumbs_measure_difference() {
        let black = Thumb::from_rgb(&vec![0; 100 * 50 * 3], 100, 50);
        let white = Thumb::from_rgb(&vec![255; 100 * 50 * 3], 100, 50);
        assert_eq!(black.distance(&black), 0.0);
        assert!((black.distance(&white) - 1.0).abs() < 1e-6);
        assert_eq!(quality(0.005), "excellent");
    }
}
