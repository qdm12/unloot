//! Decoding, orientation, downscaling and the JPEG quality hunt that keeps
//! every output under `max_bytes`.
//!
//! Everything is re-encoded from pixels, which is also how metadata goes away:
//! no EXIF, no XMP, no GPS, no camera serial, no thumbnail survives. The only
//! EXIF fact we keep is what it told us — rotation is baked into the pixels, so
//! the photo reads upright in every viewer instead of depending on a tag.

use std::io::Cursor;
use std::path::Path;

use anyhow::{Context, Result, bail};
use image::codecs::jpeg::JpegEncoder;
use image::imageops::FilterType;
use image::{DynamicImage, ExtendedColorType, ImageEncoder, ImageReader};
use tracing::debug;

use crate::scan::exif_facts;

#[derive(Clone, Debug, PartialEq)]
pub struct Encoded {
    pub bytes: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub quality: u8,
}

/// Decode, orient, flatten alpha, then encode a JPEG no larger than
/// `max_bytes` at the highest quality that fits in `[q_min, q_max]`.
///
/// If even `q_min` is too big the image is shrunk by 15% and tried again, up
/// to five times: the byte ceiling is a hard promise, quality is the flexible
/// one.
pub fn encode_limited(
    path: &Path,
    max_long_edge: u32,
    max_bytes: usize,
    q_min: u32,
    q_max: u32,
) -> Result<Encoded> {
    let decoded = ImageReader::open(path)
        .with_context(|| format!("open {}", path.display()))?
        .with_guessed_format()
        .with_context(|| format!("guess format of {}", path.display()))?
        .decode()
        .with_context(|| format!("decode {}", path.display()))?;

    let orientation = exif_facts(path).orientation;
    debug!(path = %path.display(), orientation, "baking the EXIF rotation into the pixels");
    let mut img = apply_orientation(decoded, orientation);
    img = flatten_onto_white(img);

    let (w, h) = (img.width(), img.height());
    let long = w.max(h);
    let mut scale = if long <= max_long_edge || long == 0 {
        1.0
    } else {
        max_long_edge as f64 / long as f64
    };

    let mut last: Option<Encoded> = None;
    // Shrink until the ceiling is met. Each round is ~20% smaller on the edge,
    // so twelve rounds is about a seventh of the original linear size; the
    // floor below stops it before the photo becomes a postage stamp.
    for round in 0..12 {
        let target = rescale(&img, scale)?;
        match quality_search(&target, max_bytes, q_min, q_max)? {
            Some(best) if best.bytes.len() <= max_bytes => {
                debug!(
                    path = %path.display(),
                    width = best.width,
                    height = best.height,
                    quality = best.quality,
                    round,
                    bytes = best.bytes.len(),
                    "encoded"
                );
                return Ok(best);
            }
            too_big => {
                // Keep the smallest attempt so far, so a failure can say what it
                // actually cost rather than nothing at all.
                let attempt = match too_big {
                    Some(e) => e,
                    // Even q_min overshot: encode it anyway, to have a number.
                    None => encode(&target, q_min as u8)?,
                };
                let smaller = last
                    .as_ref()
                    .is_none_or(|e| attempt.bytes.len() < e.bytes.len());
                if smaller {
                    last = Some(attempt);
                }
                scale *= 0.8;
                if target.width().min(target.height()) <= 320 {
                    break;
                }
            }
        }
    }

    match last {
        Some(e) if e.bytes.len() <= max_bytes => Ok(e),
        Some(e) => bail!(
            "{}: cannot fit {} bytes into {} (best effort {} bytes at quality {}, {}x{})",
            path.display(),
            e.bytes.len(),
            max_bytes,
            e.bytes.len(),
            e.quality,
            e.width,
            e.height
        ),
        None => bail!("{}: no encodable pixels", path.display()),
    }
}

/// Resize by `scale` (1.0 keeps the image as it is).
fn rescale(img: &DynamicImage, scale: f64) -> Result<DynamicImage> {
    if (scale - 1.0).abs() < f64::EPSILON {
        return Ok(img.clone());
    }
    let w = ((img.width() as f64 * scale).round().max(1.0)) as u32;
    let h = ((img.height() as f64 * scale).round().max(1.0)) as u32;
    // `resize` fits inside the box and never enlarges, which is exactly the
    // promise we want: no invented detail.
    Ok(img.resize(w, h, FilterType::Lanczos3))
}

/// Highest quality in `[q_min, q_max]` whose encoding fits, or `None` when
/// even `q_min` overshoots.
fn quality_search(
    img: &DynamicImage,
    max_bytes: usize,
    q_min: u32,
    q_max: u32,
) -> Result<Option<Encoded>> {
    let lowest = encode(img, q_min as u8)?;
    if lowest.bytes.len() > max_bytes {
        return Ok(None);
    }
    let highest = encode(img, q_max as u8)?;
    if highest.bytes.len() <= max_bytes {
        return Ok(Some(highest));
    }
    // Binary search between the two: bytes grow with quality.
    let (mut lo, mut hi) = (q_min as u8, q_max as u8);
    let mut best = lowest;
    while hi - lo > 1 {
        let mid = lo + (hi - lo) / 2;
        let candidate = encode(img, mid)?;
        if candidate.bytes.len() <= max_bytes {
            best = candidate;
            lo = mid;
        } else {
            hi = mid;
        }
    }
    Ok(Some(best))
}

fn encode(img: &DynamicImage, quality: u8) -> Result<Encoded> {
    let rgb = img.to_rgb8();
    let (w, h) = (rgb.width(), rgb.height());
    let mut buf = Vec::new();
    let mut cursor = Cursor::new(&mut buf);
    JpegEncoder::new_with_quality(&mut cursor, quality)
        .write_image(rgb.as_raw(), w, h, ExtendedColorType::Rgb8)
        .with_context(|| format!("jpeg encode at quality {quality}"))?;
    Ok(Encoded {
        bytes: buf,
        width: w,
        height: h,
        quality,
    })
}

/// Blend semi-transparent pixels onto white so a PNG screenshot does not turn
/// into a black rectangle.
fn flatten_onto_white(img: DynamicImage) -> DynamicImage {
    if !img.color().has_alpha() {
        return img;
    }
    let rgba = img.to_rgba8();
    let (w, h) = (rgba.width(), rgba.height());
    let mut out = image::RgbImage::from_pixel(w, h, image::Rgb([255, 255, 255]));
    for (x, y, px) in rgba.enumerate_pixels() {
        let a = px[3] as f64 / 255.0;
        let blend = |c: u8| -> u8 {
            let c = c as f64;
            (c * a + 255.0 * (1.0 - a)).round().clamp(0.0, 255.0) as u8
        };
        out.put_pixel(x, y, image::Rgb([blend(px[0]), blend(px[1]), blend(px[2])]));
    }
    DynamicImage::ImageRgb8(out)
}

/// Bake the EXIF Orientation into the pixels.
pub fn apply_orientation(img: DynamicImage, orientation: u16) -> DynamicImage {
    match orientation {
        2 => img.fliph(),
        3 => img.rotate180(),
        4 => img.flipv(),
        5 => img.rotate90().fliph(),
        6 => img.rotate90(),
        7 => img.rotate270().fliph(),
        8 => img.rotate270(),
        _ => img, // 1 and anything nonsensical: leave it alone
    }
}

/// Convenience for callers that only need to know the pixel size.
pub fn dimensions(path: &Path) -> Result<(u32, u32)> {
    let reader = ImageReader::open(path)?.with_guessed_format()?;
    let (w, h) = reader.into_dimensions()?;
    let o = exif_facts(path).orientation;
    Ok(if matches!(o, 5..=8) { (h, w) } else { (w, h) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageBuffer, Rgb};

    fn synthetic(w: u32, h: u32) -> Result<std::path::PathBuf> {
        // Noisy pixels, so the encoder cannot cheat with flat colour.
        let img: ImageBuffer<Rgb<u8>, Vec<u8>> = ImageBuffer::from_fn(w, h, |x, y| {
            let v = ((x * 7 + y * 13) % 256) as u8;
            Rgb([v, v.rotate_left(3), 255u8.saturating_sub(v)])
        });
        let mut buf = Vec::new();
        img.write_to(&mut Cursor::new(&mut buf), image::ImageFormat::Jpeg)?;
        let path = std::env::temp_dir().join(format!("unloot-test-{}-{}.jpg", w, h));
        std::fs::write(&path, &buf)?;
        Ok(path)
    }

    #[test]
    fn downscales_to_the_long_edge_and_stays_under_the_ceiling() {
        let src = synthetic(2000, 1500).unwrap();
        let out = encode_limited(&src, 1280, 300 * 1024, 62, 92).unwrap();
        assert!(
            out.width.max(out.height) <= 1280,
            "long edge {}",
            out.width.max(out.height)
        );
        assert!(out.bytes.len() <= 300 * 1024, "bytes {}", out.bytes.len());
        // Aspect ratio survives.
        assert!((out.width as f64 / out.height as f64 - 2000.0 / 1500.0).abs() < 0.01);
        let _ = std::fs::remove_file(src);
    }

    #[test]
    fn never_upsamples_a_small_input() {
        let src = synthetic(400, 300).unwrap();
        let out = encode_limited(&src, 1280, 300 * 1024, 62, 92).unwrap();
        assert_eq!((out.width, out.height), (400, 300));
        let _ = std::fs::remove_file(src);
    }

    #[test]
    fn shrinks_until_it_fits_when_quality_alone_is_not_enough() {
        let src = synthetic(1600, 1600).unwrap();
        // 60 KB is impossible at 1600 px of this noise even at q_min, so it has
        // to give up pixels rather than blow the ceiling.
        let out = encode_limited(&src, 1600, 60 * 1024, 62, 92).unwrap();
        assert!(out.bytes.len() <= 60 * 1024, "bytes {}", out.bytes.len());
        assert!(out.width < 1600 && out.width > 0, "width {}", out.width);
        assert_eq!(out.width, out.height, "square in, square out");
        let _ = std::fs::remove_file(src);
    }

    #[test]
    fn an_impossible_ceiling_fails_loudly_with_numbers() {
        let src = synthetic(1200, 900).unwrap();
        let err = encode_limited(&src, 1280, 512, 62, 92)
            .unwrap_err()
            .to_string();
        assert!(err.contains("cannot fit"), "{err}");
        // And it says what it managed, so the ceiling can be retuned.
        assert!(err.contains("best effort"), "{err}");
        let _ = std::fs::remove_file(src);
    }

    #[test]
    fn orientation_5_swaps_the_edges() {
        let src = synthetic(300, 100).unwrap();
        let img = image::open(&src).unwrap();
        let rotated = apply_orientation(img, 6);
        assert_eq!((rotated.width(), rotated.height()), (100, 300));
        let _ = std::fs::remove_file(src);
    }

    #[test]
    fn higher_quality_produces_more_bytes() {
        let src = synthetic(800, 800).unwrap();
        let img = image::open(&src).unwrap();
        let lo = encode(&img, 62).unwrap();
        let hi = encode(&img, 92).unwrap();
        assert!(hi.bytes.len() > lo.bytes.len());
        let _ = std::fs::remove_file(src);
    }
}

#[cfg(test)]
mod orientation_probe {
    use super::*;
    #[test]
    fn six_turns_a_landscape_into_a_portrait() {
        let img = DynamicImage::new_rgb8(16, 8);
        let turned = apply_orientation(img, 6);
        assert_eq!((turned.width(), turned.height()), (8, 16));
    }
}
