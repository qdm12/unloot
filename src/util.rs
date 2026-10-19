//! Small shared helpers: JSON extraction from chatty model replies, hashing,
//! civil-time formatting.

use std::fs::File;
use std::io::Read;
use std::os::fd::AsRawFd;
use std::path::Path;

use anyhow::{Context, Result, bail};
use std::str::FromStr;

use chrono::{Local, NaiveDate, NaiveDateTime, TimeZone, Utc};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// Models wrap JSON in prose or ``` fences often enough that parsing the whole
/// reply is unreliable. Try the whole string, then every balanced `{...}` /
/// `[...]` span, largest first.
pub fn extract_json(text: &str) -> Result<Value> {
    let cleaned = strip_fences(text);
    if let Ok(v) = serde_json::from_str::<Value>(cleaned.trim()) {
        return Ok(v);
    }
    let bytes: Vec<char> = cleaned.chars().collect();
    // Objects first, then arrays: every prompt asks for an object, and a reply
    // like `here you go: [{...}]` is far more likely to mean the object inside
    // than an array answer.
    let mut starts: Vec<(u8, usize)> = Vec::new();
    for (i, c) in bytes.iter().enumerate() {
        match *c {
            '{' => starts.push((0, i)),
            '[' => starts.push((1, i)),
            _ => {}
        }
    }
    starts.sort();
    for (_, s) in starts {
        if let Some(e) = match_close(&bytes, s) {
            let candidate: String = bytes[s..=e].iter().collect();
            if let Ok(v) = serde_json::from_str::<Value>(&candidate) {
                return Ok(v);
            }
        }
    }
    bail!(
        "no JSON object found in model reply ({} chars). First 200: {}",
        cleaned.trim().chars().count(),
        cleaned.trim().chars().take(200).collect::<String>()
    )
}

fn strip_fences(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.lines() {
        let t = line.trim_start();
        if t.starts_with("```") {
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// Index of the bracket closing the one at `open`, string/escape aware.
fn match_close(chars: &[char], open: usize) -> Option<usize> {
    let open_c = chars[open];
    let close_c = match open_c {
        '{' => '}',
        '[' => ']',
        _ => return None,
    };
    let mut depth = 0usize;
    let mut in_str = false;
    let mut escaped = false;
    for (i, &c) in chars.iter().enumerate().skip(open) {
        if in_str {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_str = false;
            }
            continue;
        }
        match c {
            '"' => in_str = true,
            '{' | '[' => depth += 1,
            '}' | ']' => {
                if depth == 0 {
                    return None;
                }
                depth -= 1;
                if depth == 0 {
                    return (c == close_c).then_some(i);
                }
            }
            _ => {}
        }
    }
    None
}

/// SHA-256 of a file, hex encoded. Used as the identity of a source photo.
pub fn sha256_hex(path: &Path) -> Result<String> {
    let mut f = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = f
            .read(&mut buf)
            .with_context(|| format!("read {}", path.display()))?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex::encode(h.finalize()))
}

/// Initialise logging. One subscriber for the process; a second call is a
/// no-op rather than a panic, which keeps tests that build configs harmless.
pub fn init_tracing(level: &str) {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::from_str(level.trim()).unwrap_or_else(|_| EnvFilter::new("info"));
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .without_time();
    let _ = builder.try_init();
}

/// An exclusive lock held for the life of the process.
///
/// `flock` is released by the kernel if the process dies, so a run killed mid
/// flight does not leave a lock file that blocks the next one.
pub struct FileLock {
    file: File,
}

impl FileLock {
    pub fn acquire(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("mkdir {}", parent.display()))?;
        }
        let file = File::create(path).with_context(|| format!("create lock {}", path.display()))?;
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc != 0 {
            bail!("{} is locked by another run", path.display());
        }
        Ok(FileLock { file })
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
    }
}

/// Parse an EXIF timestamp (`2026:10:04 11:07:40`) as local time.
pub fn parse_exif_time(s: &str) -> Option<i64> {
    let naive = NaiveDateTime::parse_from_str(s.trim(), "%Y:%m:%d %H:%M:%S").ok()?;
    Local
        .from_local_datetime(&naive)
        .single()
        .map(|d| d.timestamp())
}

/// Parse a phone filename timestamp (`PXL_20261004_150740746`, `P_20250524_091413`)
/// as UTC, which is how both Pixel and Samsung write them.
pub fn parse_filename_time(name: &str) -> Option<i64> {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    // `[0-9]` rather than `\d`: the regex crate is built without the Unicode
    // tables here, and `\d` would ask for them.
    let re = RE.get_or_init(|| {
        // `PXL_20261004_150740746`, `P_20250524_091413`, `2026-10-04_150740`.
        // The separators are optional because every vendor disagrees; the date
        // validity check in the caller is what stops nonsense matches.
        regex::Regex::new(r"(?:^|[^0-9])([0-9]{4})[-_]?([0-9]{2})[-_]?([0-9]{2})[-_ T]?([0-9]{2})[-_:]?([0-9]{2})[-_:]?([0-9]{2})").unwrap()
    });
    let c = re.captures(name)?;
    let (y, mo, d, h, mi, s) = (
        c[1].parse::<i32>().ok()?,
        c[2].parse::<u32>().ok()?,
        c[3].parse::<u32>().ok()?,
        c[4].parse::<u32>().ok()?,
        c[5].parse::<u32>().ok()?,
        c[6].parse::<u32>().ok()?,
    );
    let naive = NaiveDate::from_ymd_opt(y, mo, d)?.and_hms_opt(h, mi, s)?;
    Some(Utc.from_utc_datetime(&naive).timestamp())
}

/// Human readable byte count for logs.
pub fn kb(bytes: usize) -> String {
    format!("{:.0} KB", bytes as f64 / 1024.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_plain_and_fenced_and_padded_json() {
        assert_eq!(extract_json(r#"{"a":1}"#).unwrap()["a"], 1);
        assert_eq!(extract_json("```json\n{\"a\":2}\n```").unwrap()["a"], 2);
        assert_eq!(
            extract_json("Sure! Here you go: {\"a\":3} hope that helps").unwrap()["a"],
            3
        );
        assert_eq!(extract_json("text [{\"a\":4}] more").unwrap()["a"], 4);
    }

    #[test]
    fn extracts_nested_braces_inside_strings() {
        let raw = r#"noise {"note":"brace { in it","n":5} tail"#;
        let v = extract_json(raw).unwrap();
        assert_eq!(v["note"], "brace { in it");
        assert_eq!(v["n"], 5);
    }

    #[test]
    fn rejects_junk() {
        assert!(extract_json("no json here").is_err());
    }

    #[test]
    fn parses_both_timestamp_flavours() {
        assert!(parse_exif_time("2026:10:04 11:07:40").is_some());
        // Pixel writes local wall time with milliseconds; checked against `date -u`.
        assert_eq!(
            parse_filename_time("PXL_20261004_150740746.jpg").unwrap(),
            1791126460
        );
        // Samsung writes a plain second, no milliseconds.
        assert_eq!(
            parse_filename_time("P_20250524_091413.jpg").unwrap(),
            1748078053
        );
        assert!(parse_filename_time("IMG_1234.jpg").is_none());
        assert!(parse_filename_time("2026-10-04_150740.jpg").is_some());
    }

    #[test]
    fn match_close_prefers_the_outmost_span() {
        let chars: Vec<char> = r#"{"a":{"b":1},"c":2}"#.chars().collect();
        assert_eq!(match_close(&chars, 0), Some(chars.len() - 1));
    }
}
