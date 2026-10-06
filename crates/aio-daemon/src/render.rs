//! Turns sources into the 480×480 baseline JPEGs the panel accepts.

use std::path::Path;

use aio_proto::{HEIGHT, WIDTH};
use anyhow::{Context, Result};
use image::imageops::FilterType;
use image::{DynamicImage, Rgb, RgbImage, RgbaImage};
use jpeg_encoder::{ColorType, Encoder, SamplingFactor};

pub const JPEG_QUALITY: u8 = 90;

/// Encodes 480×480 packed RGB as baseline JPEG with 4:2:0 chroma, matching
/// the frames MSI's driver sends (and Pillow's defaults).
pub fn encode_rgb(rgb: &[u8]) -> Result<Vec<u8>> {
    anyhow::ensure!(rgb.len() == WIDTH as usize * HEIGHT as usize * 3, "expected 480x480 RGB");
    let mut out = Vec::with_capacity(64 * 1024);
    let mut encoder = Encoder::new(&mut out, JPEG_QUALITY);
    encoder.set_sampling_factor(SamplingFactor::F_2_2);
    encoder.encode(rgb, WIDTH, HEIGHT, ColorType::Rgb)?;
    Ok(out)
}

pub fn solid_color(rgb: [u8; 3]) -> Result<Vec<u8>> {
    encode_rgb(&rgb.repeat(WIDTH as usize * HEIGHT as usize))
}

/// Loads an image file (first frame for animations) and stretches it to fill
/// the panel.
pub fn image_file(path: &Path) -> Result<Vec<u8>> {
    let img = image::open(path).with_context(|| format!("opening {}", path.display()))?;
    panel_jpeg(img)
}

/// The largest centered square of a `width`×`height` picture: (x, y, side).
pub fn center_square(width: u32, height: u32) -> (u32, u32, u32) {
    let side = width.min(height);
    ((width - side) / 2, (height - side) / 2, side)
}

/// Composites transparency over black, crops the centered square, scales it
/// (up or down) to 480×480 and encodes.
pub fn panel_jpeg(img: DynamicImage) -> Result<Vec<u8>> {
    let rgb = if img.color().has_alpha() {
        DynamicImage::ImageRgb8(over_black(&img.into_rgba8()))
    } else {
        img
    };
    let (x, y, side) = center_square(rgb.width(), rgb.height());
    let rgb = rgb
        .crop_imm(x, y, side, side)
        .resize_exact(WIDTH.into(), HEIGHT.into(), FilterType::CatmullRom)
        .into_rgb8();
    encode_rgb(rgb.as_raw())
}

/// `jpeg` (a 480×480 panel frame) at each brightness in `levels`, given in
/// 1/`steps` units (0 = black, `steps` = unchanged).
fn brightness_ramp(jpeg: &[u8], steps: u32, levels: impl Iterator<Item = u32>) -> Result<Vec<Vec<u8>>> {
    let rgb = image::load_from_memory(jpeg)?
        .resize_exact(WIDTH.into(), HEIGHT.into(), FilterType::Triangle)
        .into_rgb8()
        .into_raw();
    let mut scaled = vec![0u8; rgb.len()];
    levels
        .map(|level| {
            for (out, &v) in scaled.iter_mut().zip(&rgb) {
                *out = (u32::from(v) * level / steps) as u8;
            }
            encode_rgb(&scaled)
        })
        .collect()
}

/// `steps` frames that fade `jpeg` to black; the last one is pure black.
pub fn fade_to_black(jpeg: &[u8], steps: u32) -> Result<Vec<Vec<u8>>> {
    brightness_ramp(jpeg, steps, (0..steps).rev())
}

/// `steps` frames that fade from black towards `jpeg`; the first is pure
/// black. Send `jpeg` itself afterwards to finish at full brightness.
pub fn fade_from_black(jpeg: &[u8], steps: u32) -> Result<Vec<Vec<u8>>> {
    brightness_ramp(jpeg, steps, 0..steps)
}

fn over_black(rgba: &RgbaImage) -> RgbImage {
    let mul = |c: u8, a: u8| ((c as u16 * a as u16 + 127) / 255) as u8;
    RgbImage::from_fn(rgba.width(), rgba.height(), |x, y| {
        let [r, g, b, a] = rgba.get_pixel(x, y).0;
        Rgb([mul(r, a), mul(g, a), mul(b, a)])
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Returns the SOF marker and its component sampling factors.
    fn frame_info(jpeg: &[u8]) -> (u8, Vec<(u8, u8)>) {
        let mut i = 2;
        loop {
            assert_eq!(jpeg[i], 0xFF);
            let marker = jpeg[i + 1];
            let len = u16::from_be_bytes([jpeg[i + 2], jpeg[i + 3]]) as usize;
            if (0xC0..=0xC2).contains(&marker) {
                let n = jpeg[i + 9] as usize;
                let comps = (0..n).map(|k| {
                    let s = jpeg[i + 11 + 3 * k];
                    (s >> 4, s & 0xF)
                });
                return (marker, comps.collect());
            }
            i += 2 + len;
        }
    }

    #[test]
    fn solid_color_is_baseline_420_jfif() {
        let jpeg = solid_color([255, 0, 0]).unwrap();
        assert_eq!(&jpeg[..4], &[0xFF, 0xD8, 0xFF, 0xE0]); // SOI + APP0
        assert_eq!(&jpeg[6..11], b"JFIF\0");
        let (sof, comps) = frame_info(&jpeg);
        assert_eq!(sof, 0xC0, "must be baseline");
        assert_eq!(comps, [(2, 2), (1, 1), (1, 1)]);

        let decoded = image::load_from_memory(&jpeg).unwrap().to_rgb8();
        assert_eq!(decoded.dimensions(), (480, 480));
        let px = decoded.get_pixel(240, 240).0;
        assert!(px[0] > 245 && px[1] < 10 && px[2] < 10, "{px:?}");
    }

    #[test]
    fn center_square_math() {
        assert_eq!(center_square(1920, 1080), (420, 0, 1080));
        assert_eq!(center_square(100, 300), (0, 100, 100));
        assert_eq!(center_square(480, 480), (0, 0, 480));
        assert_eq!(center_square(101, 100), (0, 0, 100));
    }

    /// A `w`×`h` image: blue centered square, red everywhere else.
    fn bars(w: u32, h: u32) -> DynamicImage {
        let (x0, y0, side) = center_square(w, h);
        DynamicImage::ImageRgb8(RgbImage::from_fn(w, h, |x, y| {
            let inside = (x0..x0 + side).contains(&x) && (y0..y0 + side).contains(&y);
            if inside { Rgb([0, 0, 255]) } else { Rgb([255, 0, 0]) }
        }))
    }

    #[test]
    fn crops_center_square_of_wide_and_tall_images() {
        for (w, h) in [(160, 90), (90, 160), (2000, 500), (30, 20)] {
            let jpeg = panel_jpeg(bars(w, h)).unwrap();
            let img = image::load_from_memory(&jpeg).unwrap().to_rgb8();
            assert_eq!(img.dimensions(), (480, 480));
            for (x, y) in [(4, 4), (475, 4), (4, 475), (475, 475), (240, 240)] {
                let [r, _, b] = img.get_pixel(x, y).0;
                assert!(b > 200 && r < 60, "{w}x{h}: pixel ({x},{y}) = {:?}", img.get_pixel(x, y).0);
            }
        }
    }

    #[test]
    fn image_file_is_resized_to_panel() {
        let dir = std::env::temp_dir().join(format!("aio-render-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("small.png");
        image::RgbaImage::from_pixel(64, 32, image::Rgba([0, 0, 255, 255])).save(&path).unwrap();

        let jpeg = image_file(&path).unwrap();
        let decoded = image::load_from_memory(&jpeg).unwrap().to_rgb8();
        assert_eq!(decoded.dimensions(), (480, 480));
        assert!(decoded.get_pixel(10, 10).0[2] > 240);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn fades_to_black() {
        let frames = fade_to_black(&solid_color([200, 100, 50]).unwrap(), 4).unwrap();
        assert_eq!(frames.len(), 4);
        let reds: Vec<u8> = frames
            .iter()
            .map(|f| image::load_from_memory(f).unwrap().to_rgb8().get_pixel(240, 240).0[0])
            .collect();
        assert!(reds.windows(2).all(|w| w[0] > w[1]), "{reds:?}");
        assert!((140..=160).contains(&reds[0]), "{reds:?}"); // 3/4 of 200
        assert!(reds[3] <= 2, "{reds:?}");
    }

    #[test]
    fn fades_from_black() {
        let frames = fade_from_black(&solid_color([200, 100, 50]).unwrap(), 4).unwrap();
        assert_eq!(frames.len(), 4);
        let reds: Vec<u8> = frames
            .iter()
            .map(|f| image::load_from_memory(f).unwrap().to_rgb8().get_pixel(240, 240).0[0])
            .collect();
        assert!(reds[0] <= 2, "{reds:?}");
        assert!(reds.windows(2).all(|w| w[0] < w[1]), "{reds:?}");
        assert!((140..=160).contains(&reds[3]), "{reds:?}"); // 3/4 of 200
    }

    #[test]
    fn transparency_becomes_black() {
        let half_blue = image::RgbaImage::from_pixel(8, 8, image::Rgba([0, 0, 255, 128]));
        let jpeg = panel_jpeg(DynamicImage::ImageRgba8(half_blue)).unwrap();
        let px = image::load_from_memory(&jpeg).unwrap().to_rgb8().get_pixel(240, 240).0;
        assert!((120..=136).contains(&px[2]) && px[0] < 8, "{px:?}");
    }
}
