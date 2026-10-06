# Loop finder (`aio-loop`)

Makes a seamless loop out of a video: you mark roughly where the loop should
start and end, it finds the frames that join without a visible jump, and
saves `<name>_loop.mp4`.

- `aio-loop.exe` — the window (also `aio-loop.exe <video>`; drop a file on it).
- `aio-loop-cli.exe` — the same from a terminal: `info`, `find`, `cut`, `auto`.
- Needs `ffmpeg`/`ffprobe` (`AIO_FFMPEG`, else PATH), like the daemon.

## How it works

1. `ffprobe` reads size, frame rate, duration and frame count.
2. `ffmpeg` decodes a window of frames (±radius, default 2 s, max 5 s) around
   the start mark and around the end mark, at 160 px wide.
3. Each frame is reduced to a 32×32 RGB thumbnail. For every start frame `A`
   and end frame `B` the score is the mean difference of `A+k` vs `B+k` for
   `k = -2..2`, so the motion must continue across the cut, not just one frame
   look alike. All pairwise distances are computed once, in parallel.
4. The best pairs are listed (at least 0.2 s apart, loops at least 0.25 s
   long), with a quality label: excellent < 1 %, good < 2.5 %, fair < 5 %.
5. Saving cuts frames `[A, B)`: frame `B` matches `A`, so it is left out and
   playback wraps from `B-1` to `A`. Re-encoded with H.264 (CRF 18, yuv420p,
   faststart), audio optional (off by default). Frame-accurate: ffmpeg seeks a
   quarter frame before `A` and writes exactly `B - A` frames.

Assumes a constant frame rate (almost all files). Times can be typed as
seconds or `m:ss.mmm`.

## Tests

- Unit tests: timecodes, probe parsing (including rotated phone videos),
  thumbnails, the matcher (exact period found, motion direction matters,
  minimum length, distinct candidates).
- `tests/end_to_end.rs` (needs ffmpeg): generates a clip with a known
  90-frame period, checks the search finds a multiple of it, that the cut has
  exactly `B - A` frames, and that the seam is no bigger than a normal step.
