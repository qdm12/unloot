//! Discovery: find the photos to process, and the best available capture time
//! for each. Sub-directories are never entered — that is what makes a second
//! run over the same tree a no-op instead of a re-processing of output.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use tracing::{debug, warn};

use crate::util;

/// Extensions we will decode. Anything else in the folder is left in place.
const IMAGE_EXTS: &[&str] = &["jpg", "jpeg", "png", "webp", "bmp", "gif"];

/// Where a photo's timestamp came from. Only ever a hint: the model decides
/// whether a gap means a new item.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TimeSource {
    Exif,
    FileName,
    Modified,
    None,
}

#[derive(Clone, Debug)]
pub struct SourceImage {
    pub path: PathBuf,
    /// File name, kept for the output copy as-is.
    pub name: String,
    /// 1-based position in capture order, the id used in prompts and logs.
    pub id: usize,
    pub epoch: Option<i64>,
    pub time_source: TimeSource,
    pub bytes: u64,
    pub sha256: String,
}

impl SourceImage {
    /// Seconds between this photo and the previous one, when both are timed.
    pub fn gap_from(&self, prev: Option<&SourceImage>) -> Option<f64> {
        match (prev, self.epoch) {
            (Some(p), Some(t)) => p.epoch.map(|pt| (t - pt) as f64),
            _ => None,
        }
    }
}

/// EXIF facts we care about: when it was taken, and how it is oriented.
#[derive(Debug, Default)]
pub struct ExifFacts {
    pub taken_at: Option<i64>,
    /// 1..=8, per EXIF Orientation. 1 means "upright".
    pub orientation: u16,
}

/// Read a couple of EXIF fields, tolerating files with no EXIF at all.
pub fn exif_facts(path: &Path) -> ExifFacts {
    let mut out = ExifFacts {
        orientation: 1,
        taken_at: None,
    };
    let probe = |file: &mut std::fs::File| -> Result<(Option<i64>, u16), exif::Error> {
        let mut buf = std::io::BufReader::new(file);
        let meta = exif::Reader::new().read_from_container(&mut buf)?;
        let orientation = meta
            .get_field(exif::Tag::Orientation, exif::In::PRIMARY)
            .and_then(|f| f.value.get_uint(0))
            .map(|v| v as u16)
            .unwrap_or(1);
        let taken = meta
            .get_field(exif::Tag::DateTimeOriginal, exif::In::PRIMARY)
            .and_then(|f| match &f.value {
                exif::Value::Ascii(rows) => {
                    rows.first().map(|r| String::from_utf8_lossy(r).to_string())
                }
                _ => None,
            })
            .and_then(|s| util::parse_exif_time(&s));
        Ok((taken, orientation))
    };
    if let Ok(mut f) = std::fs::File::open(path)
        && let Ok((taken, orientation)) = probe(&mut f)
    {
        out.taken_at = taken;
        out.orientation = orientation;
    }
    out
}

fn modified_epoch(path: &Path) -> Option<i64> {
    path.metadata()
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
}

/// True for `photo.JPG`, false for `.txt`, `.DS_Store`, directories, etc.
/// One directory entry as a photo, or None if it is not one.
fn one(entry: &std::fs::DirEntry) -> Result<Option<SourceImage>> {
    let path = entry.path();
    if !entry.file_type()?.is_file() {
        return Ok(None);
    }
    if !is_image(&path) {
        debug!(path = %path.display(), "skipping non-image file");
        return Ok(None);
    }
    let name = entry.file_name().to_string_lossy().to_string();
    let exif = exif_facts(&path);
    let epoch = exif
        .taken_at
        .or_else(|| util::parse_filename_time(&name))
        .or_else(|| modified_epoch(&path));
    let time_source = if exif.taken_at.is_some() {
        TimeSource::Exif
    } else if util::parse_filename_time(&name).is_some() {
        TimeSource::FileName
    } else if modified_epoch(&path).is_some() {
        TimeSource::Modified
    } else {
        TimeSource::None
    };
    let sha256 = util::sha256_hex(&path)?;
    Ok(Some(SourceImage {
        path,
        name,
        id: 0,
        epoch,
        time_source,
        bytes: entry.metadata().map(|m| m.len()).unwrap_or(0),
        sha256,
    }))
}

fn is_image(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| IMAGE_EXTS.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

/// List candidate photos in `dir`, capture-ordered.
///
/// Order is: best available timestamp (EXIF, then file name, then mtime),
/// ties broken by name. Files whose name carries a timestamp sort among the
/// others by that timestamp, so a folder mixing two phone exports still reads
/// as two separate sessions.
pub fn scan(dir: &Path) -> Result<Vec<SourceImage>> {
    let mut items: Vec<SourceImage> = Vec::new();

    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        match one(&entry)? {
            Some(img) => items.push(img),
            None => continue,
        }
    }

    // Untimed files keep their alphabetical position at the end.
    items.sort_by(|a, b| match (a.epoch, b.epoch) {
        (Some(x), Some(y)) => x.cmp(&y),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => a.name.cmp(&b.name),
    });
    if items.iter().any(|i| i.time_source == TimeSource::Modified) {
        let mixed = items.iter().any(|i| i.time_source == TimeSource::Exif);
        if mixed {
            tracing::warn!(
                "some photos have no EXIF or filename timestamp; their modification time puts them in capture order, which may not match the real order"
            );
        }
    }
    for (i, item) in items.iter_mut().enumerate() {
        item.id = i + 1;
    }
    Ok(items)
}

/// Photos that have settled: unchanged for `settle_secs`, so we do not pick up
/// a file still being copied or AirDropped.
/// A folder in the output directory that already holds one item.
///
/// Produced when the sorting was done by hand, or on another machine: the name
/// is the folder's own and is never questioned, and the photos inside are read
/// only — never re-encoded, moved, or removed.
#[derive(Clone, Debug)]
pub struct ItemDir {
    pub name: String,
    pub path: PathBuf,
    pub images: Vec<SourceImage>,
}

/// How deep to look for photos inside an item folder.
const ITEM_DIR_DEPTH: usize = 3;

/// The item folders already present in `dir`, in name order.
///
/// Dot-prefixed folders are unloot's own bookkeeping and are skipped, as are
/// folders holding no images at all. A folder that cannot be read is skipped
/// with a warning: one locked directory should not cost the user the whole run.
pub fn scan_item_dirs(dir: &Path) -> Result<Vec<ItemDir>> {
    let mut found: Vec<ItemDir> = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        match collect_images(&entry.path(), ITEM_DIR_DEPTH) {
            Ok(images) if images.is_empty() => {
                debug!(dir = %entry.path().display(), "folder holds no photos; nothing to describe");
            }
            Ok(mut images) => {
                for (i, img) in images.iter_mut().enumerate() {
                    img.id = i + 1;
                }
                found.push(ItemDir {
                    name,
                    path: entry.path(),
                    images,
                });
            }
            Err(e) => {
                warn!(dir = %entry.path().display(), error = %format!("{e:#}"), "cannot read this folder; skipping it");
            }
        }
    }
    found.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(found)
}

/// Photos under `dir`, down to `depth` levels, in name order.
fn collect_images(dir: &Path, depth: usize) -> Result<Vec<SourceImage>> {
    let mut items: Vec<SourceImage> = Vec::new();
    let mut deeper: Vec<PathBuf> = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        if entry.file_type()?.is_dir() {
            if depth > 0 && !name.starts_with('.') {
                deeper.push(entry.path());
            }
            continue;
        }
        if let Some(img) = one(&entry)? {
            items.push(img);
        }
    }
    for sub in deeper {
        items.extend(collect_images(&sub, depth - 1)?);
    }
    items.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(items)
}

pub fn settled(items: &[SourceImage], settle_secs: usize, now: SystemTime) -> Vec<SourceImage> {
    if settle_secs == 0 {
        return items.to_vec();
    }
    items
        .iter()
        .filter(|i| {
            i.path
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| now.duration_since(t).ok())
                .map(|age| age.as_secs() as usize >= settle_secs)
                .unwrap_or(false)
        })
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_filter_is_case_insensitive_and_whitelist_only() {
        assert!(is_image(Path::new("/x/a.JPG")));
        assert!(is_image(Path::new("/x/a.jpeg")));
        assert!(!is_image(Path::new("/x/a.txt")));
        assert!(!is_image(Path::new("/x/noext")));
    }

    /// The minimum EXIF a test can carry: one APP1, one tag. If kamadak will not
    /// read this, the end-to-end orientation test is testing the fixture rather
    /// than the pipeline, and we need to know which.
    #[test]
    fn reads_orientation_from_a_minimal_app1() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("p.jpg");
        let img: image::RgbImage = image::RgbImage::from_pixel(8, 4, image::Rgb([9, 9, 9]));
        let mut buf = Vec::new();
        img.write_to(
            &mut std::io::Cursor::new(&mut buf),
            image::ImageFormat::Jpeg,
        )
        .unwrap();
        buf.splice(2..2, app1_orientation_6());
        std::fs::write(&path, &buf).unwrap();
        let facts = exif_facts(&path);
        assert_eq!(
            facts.orientation, 6,
            "orientation not read from a one-tag APP1"
        );
    }

    fn app1_orientation_6() -> Vec<u8> {
        let mut seg = vec![0xFF, 0xE1];
        let body: Vec<u8> = b"Exif\0\0"
            .iter()
            .copied()
            .chain([
                b'I', b'I', 0x2A, 0x00, 0x08, 0x00, 0x00, 0x00, // IFD0 at offset 8
                0x01, 0x00, // one entry
                0x12, 0x01, 0x03, 0x00, // 0x0112 Orientation, SHORT
                0x01, 0x00, 0x00, 0x00, 0x06, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            ])
            .collect();
        let len = (body.len() + 2) as u16;
        seg.push((len >> 8) as u8);
        seg.push(len as u8);
        seg.extend(body);
        seg
    }

    /// The whole path a rotated photo takes: read the tag, bake it, re-encode.
    /// Big and noisy enough to force the shrink loop, which is how the real
    /// photos arrive. Checking the tag alone and the rotator alone would let a
    /// mismatch between the two slip through.
    #[test]
    fn encode_limited_bakes_a_one_tag_exif_rotation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("p.jpg");
        let img: image::ImageBuffer<image::Rgb<u8>, Vec<u8>> =
            image::ImageBuffer::from_fn(1600, 1200, |x, y| {
                let n = (x * 7 + y * 11) % 251;
                image::Rgb([(n * 3) as u8, (n * 5 + 40) as u8, (n * 7 + 90) as u8])
            });
        let mut buf = Vec::new();
        img.write_to(
            &mut std::io::Cursor::new(&mut buf),
            image::ImageFormat::Jpeg,
        )
        .unwrap();
        buf.splice(2..2, app1_orientation_6());
        std::fs::write(&path, &buf).unwrap();
        assert_eq!(exif_facts(&path).orientation, 6);
        let out = crate::imageops::encode_limited(&path, 1280, 307_200, 62, 92).unwrap();
        assert!(
            out.height > out.width,
            "not portrait: {}x{}",
            out.width,
            out.height
        );
    }

    #[test]
    fn gap_is_signed_seconds() {
        let mut a = SourceImage {
            path: PathBuf::from("a"),
            name: "a".into(),
            id: 1,
            epoch: Some(100),
            time_source: TimeSource::None,
            bytes: 0,
            sha256: String::new(),
        };
        let b = SourceImage {
            epoch: Some(160),
            ..a.clone()
        };
        assert_eq!(b.gap_from(Some(&a)), Some(60.0));
        assert_eq!(a.gap_from(None), None);
        a.epoch = None;
        assert_eq!(b.gap_from(Some(&a)), None);
    }
}
