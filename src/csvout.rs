//! The CSV. One row per item, written with a fresh timestamp in the name so a
//! run never overwrites the listing you made yesterday.
//!
//! Two headers follow the currency: the price column says which currency it
//! is, and weight is in ounces for USD (how Vinted's US buyers think of it) and
//! grams everywhere else.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use tracing::debug;

use crate::config::Config;

/// Columns in file order. `{price}` and `{weight}` are filled by [`headers`].
///
/// Deliberately only the listing itself. How many photos an item has, what it
/// was called on disk, and how big the JPEGs came out are all recoverable from
/// the folder and from `.unloot/state.json`, and a row you can paste straight
/// into Vinted is a row you can read at a glance.
const HEADER_TEMPLATE: &[&str] = &[
    "name", "category", "brand", "color", "size", "{price}", "{weight}",
];

/// The header row for one currency.
pub fn headers(cfg: &Config) -> Vec<String> {
    HEADER_TEMPLATE
        .iter()
        .map(|h| match *h {
            "{price}" => format!("price ({})", cfg.currency),
            "{weight}" => cfg.weight_column(),
            other => (*other).to_string(),
        })
        .collect()
}

/// One item, as the listing spreadsheet wants it.
#[derive(Clone, Debug, Default)]
pub struct Row {
    pub name: String,
    pub category: String,
    pub brand: String,
    pub color: String,
    pub size: String,
    pub price: String,
    /// Always kept in both units; the currency decides which one is written.
    pub weight_oz: String,
    pub weight_g: String,
    /// Not written to the CSV — the dry run prints them, and only it.
    pub photos: usize,
    pub source_files: String,
    pub width: u32,
    pub height: u32,
    pub bytes: usize,
}

impl Row {
    pub fn fields(&self, cfg: &Config) -> Vec<String> {
        let weight = if cfg.uses_ounces() {
            self.weight_oz.clone()
        } else {
            self.weight_g.clone()
        };
        vec![
            self.name.clone(),
            self.category.clone(),
            self.brand.clone(),
            self.color.clone(),
            self.size.clone(),
            self.price.clone(),
            weight,
        ]
    }

    /// Rebuild a saved row when resuming. The row was written under the same
    /// column layout, so the weight lands in the unit that currency uses. A row
    /// saved by an older version may carry the columns this one no longer
    /// writes; they are past the end of what is read here, so they are ignored.
    pub fn from_fields(f: &[String], cfg: &Config) -> Self {
        let get = |i: usize| f.get(i).cloned().unwrap_or_default();
        let weight = get(6);
        let (oz, g) = if cfg.uses_ounces() {
            (weight.clone(), recompute_g(&weight))
        } else {
            (recompute_oz(&weight), weight.clone())
        };
        Row {
            name: get(0),
            category: get(1),
            brand: get(2),
            color: get(3),
            size: get(4),
            price: get(5),
            weight_oz: oz,
            weight_g: g,
            ..Default::default()
        }
    }
}

const G_PER_OZ: f64 = 28.349_523_125;

fn recompute_g(oz: &str) -> String {
    oz.parse::<f64>()
        .map(|v| format!("{:.0}", v * G_PER_OZ))
        .unwrap_or_default()
}

fn recompute_oz(g: &str) -> String {
    g.parse::<f64>()
        .map(|v| format!("{:.1}", v / G_PER_OZ))
        .unwrap_or_default()
}

/// `yymmdd_hhmmss`, the shape the summary file has always used.
pub fn timestamp(now: &chrono::DateTime<chrono::Local>) -> String {
    now.format("%y%m%d_%H%M%S").to_string()
}

/// Expand `{ts}` in `--csv-name`. A missing extension is added rather than
/// complained about, because the file is still meant to be opened in a sheet.
pub fn resolve_name(template: &str, ts: &str, output_dir: &Path) -> PathBuf {
    let mut name = template.replace("{ts}", ts).replace("{timestamp}", ts);
    if !name.to_ascii_lowercase().ends_with(".csv") {
        name.push_str(".csv");
    }
    let path = Path::new(&name);
    if path.is_absolute() || path.components().count() > 1 {
        // Only a bare file name belongs in the output directory.
        PathBuf::from(name)
    } else {
        output_dir.join(name)
    }
}

/// Write the summary. Rows are written in the order given, which is capture
/// order for this run and name order for rows carried over from earlier runs.
pub fn write(path: &Path, cfg: &Config, rows: &[Row]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let mut w = csv::Writer::from_path(path)
        .with_context(|| format!("create summary {}", path.display()))?;
    w.write_record(headers(cfg))
        .context("write summary header")?;
    for row in rows {
        w.write_record(row.fields(cfg))
            .with_context(|| format!("write row for {:?}", row.name))?;
    }
    w.flush().context("flush summary")?;
    debug!(path = %path.display(), rows = rows.len(), "summary written");
    Ok(())
}

/// The summary this folder already holds, if any: the newest file matching the
/// `--csv-name` pattern.
///
/// Used to tell which items are already listed when there is no ledger to ask —
/// a folder sorted on one machine and described on another, or a `.unloot`
/// directory deleted on purpose.
pub fn latest_summary(cfg: &Config, dir: &Path) -> Option<PathBuf> {
    let name = &cfg.csv_name;
    let mark = name
        .find("{ts}")
        .map(|i| (i, "{ts}".len()))
        .or_else(|| name.find("{timestamp}").map(|i| (i, "{timestamp}".len())));
    let (prefix, suffix) = match mark {
        Some((i, len)) => (name[..i].to_string(), name[i + len..].to_string()),
        None => (name.clone(), String::new()),
    };

    let mut best: Option<(SystemTime, PathBuf)> = None;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let file = entry.file_name().to_string_lossy().to_string();
        if file.len() <= prefix.len() + suffix.len()
            || !file.starts_with(&prefix)
            || !file.ends_with(&suffix)
            || !entry.path().is_file()
        {
            continue;
        }
        let when = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(UNIX_EPOCH);
        if best.as_ref().is_none_or(|(w, _)| when > *w) {
            best = Some((when, entry.path()));
        }
    }
    best.map(|(_, path)| path)
}

/// Read a summary back, to check it matches what we meant to write.
pub fn read_rows(path: &Path, cfg: &Config) -> Result<Vec<Row>> {
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut rdr = csv::ReaderBuilder::new()
        .has_headers(true)
        .from_reader(file);
    let mut out = Vec::new();
    for record in rdr.records() {
        let r = record.context("read summary row")?;
        out.push(Row::from_fields(
            &r.iter().map(Into::into).collect::<Vec<String>>(),
            cfg,
        ));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(currency: &str) -> Config {
        let args = vec!["--llm-url".to_string(), "http://x/v1".to_string()];
        let lookup = |_k: &str| None;
        let mut c = crate::config::resolve_from(&args, &lookup).unwrap();
        c.currency = currency.into();
        c
    }

    #[test]
    fn headers_name_the_currency_and_the_weight_unit() {
        let usd = headers(&cfg("USD"));
        assert!(usd.iter().any(|h| h == "price (USD)"), "{usd:?}");
        assert!(usd.iter().any(|h| h == "weight (oz)"), "{usd:?}");
        let eur = headers(&cfg("EUR"));
        assert!(eur.iter().any(|h| h == "price (EUR)"), "{eur:?}");
        assert!(eur.iter().any(|h| h == "weight (g)"), "{eur:?}");
        assert_eq!(usd.len(), eur.len());
    }

    #[test]
    fn name_template_expands_and_keeps_csv_extension() {
        let out = Path::new("/work/out");
        assert_eq!(
            resolve_name("summary_{ts}.csv", "260215_101500", out),
            PathBuf::from("/work/out/summary_260215_101500.csv")
        );
        assert_eq!(
            resolve_name("listings", "260215_101500", out),
            PathBuf::from("/work/out/listings.csv")
        );
        assert_eq!(
            resolve_name("{ts}.csv", "260215_101500", out),
            PathBuf::from("/work/out/260215_101500.csv")
        );
    }

    #[test]
    fn round_trip_a_row_with_commas_quotes_and_newlines() {
        let dir = std::env::temp_dir().join(format!("unloot-csv-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.csv");
        let usd = cfg("USD");
        let rows = vec![Row {
            name: "Sweater, wool \"cream\"".into(),
            category: "Sweater".into(),
            brand: "Unbranded".into(),
            color: "cream".into(),
            size: "M".into(),
            price: "14.00".into(),
            weight_oz: "8.5".into(),
            weight_g: "241".into(),
            photos: 4,
            source_files: "IMG_1.jpg,IMG_2.jpg".into(),
            width: 1280,
            height: 720,
            bytes: 148000,
        }];
        write(&path, &usd, &rows).unwrap();
        let back = read_rows(&path, &usd).unwrap();
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].name, "Sweater, wool \"cream\"");
        assert_eq!(back[0].weight_oz, "8.5");
        // The columns this version does not write stay empty rather than
        // picking up the wrong values from an old, wider row.
        assert_eq!(back[0].source_files, "");
        assert_eq!(back[0].photos, 0);
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().next().unwrap().split(',').count(), 7, "{text}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn euros_get_grams_instead_of_ounces() {
        let dir = std::env::temp_dir().join(format!("unloot-csv-eur-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.csv");
        let eur = cfg("EUR");
        let row = Row {
            name: "Jeans".into(),
            category: "Jeans".into(),
            brand: "Levi's".into(),
            color: "blue".into(),
            size: "32".into(),
            price: "20.00".into(),
            weight_oz: "14.1".into(),
            weight_g: "400".into(),
            photos: 1,
            source_files: "IMG_9.jpg".into(),
            width: 1280,
            height: 960,
            bytes: 90000,
        };
        write(&path, &eur, std::slice::from_ref(&row)).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("400"), "{text}");
        assert!(
            !text.contains(",14.1,"),
            "grams expected, ounces leaked: {text}"
        );
        let back = read_rows(&path, &eur).unwrap();
        assert_eq!(back[0].weight_g, "400");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_newest_summary_of_a_folder_wins() {
        let dir = std::env::temp_dir().join(format!("unloot-latest-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let usd = cfg("USD");
        for (name, when) in [
            ("summary_260101_010101.csv", UNIX_EPOCH),
            (
                "summary_260202_020202.csv",
                UNIX_EPOCH + std::time::Duration::from_secs(500),
            ),
        ] {
            write(&dir.join(name), &usd, &[]).unwrap();
            filetime_set(&dir.join(name), when);
        }
        std::fs::write(dir.join("notes.csv"), b"x").unwrap();
        std::fs::create_dir_all(dir.join("Sweater grey")).unwrap();
        let found = latest_summary(&usd, &dir).unwrap();
        assert_eq!(
            found.file_name().unwrap().to_string_lossy(),
            "summary_260202_020202.csv"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    fn filetime_set(path: &Path, when: SystemTime) {
        let f = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        f.set_modified(when).unwrap();
    }
}
