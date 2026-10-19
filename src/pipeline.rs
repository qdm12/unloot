//! The pipeline: scan, settle, cluster, catalogue, write, delete.
//!
//! The order of operations is the safety story, so it is worth stating once
//! here:
//!
//! 1. nothing is deleted until every compressed copy of that item exists on
//!    disk and has been read back and checked;
//! 2. a copy appears in its final place via a rename from a temporary file in
//!    the same directory, so a photo is never half-written under its real name;
//! 3. the ledger is updated after each item, so an interrupted run resumes
//!    where it stopped instead of starting over or double-listing.
//!
//! There is no "undo", and I would not want one: an undo tool implies we are
//! comfortable losing the originals. The equivalents are `--dry-run` and
//! `--keep-source`.

use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};
use tracing::{debug, info, warn};

use crate::catalog::{self, Item, Meta};
use crate::config::Config;
use crate::csvout::{self, Row};
use crate::imageops;
use crate::llm::{self, Llm};
use crate::scan::{self, SourceImage, settled};
use crate::state::{Processed, State};
use crate::util::{self, FileLock};

/// Grams per ounce.
const G_PER_OZ: f64 = 28.349_523_125;

#[derive(Debug, Default)]
pub struct Report {
    pub items: usize,
    pub photos: usize,
    pub bytes_out: usize,
    /// Folders that were already sorted and only needed describing.
    pub described: usize,
    pub csv: Option<PathBuf>,
}

pub fn run(cfg: &Config) -> Result<Report> {
    let input = cfg.input_dir.clone();
    let output = cfg.output_dir.clone();
    if !input.is_dir() {
        bail!("input directory {} is not a directory", input.display());
    }
    if !cfg.dry_run {
        std::fs::create_dir_all(&output)
            .with_context(|| format!("create output directory {}", output.display()))?;
    }

    // Two runs against one output directory would race over names and delete
    // each other's work, so only one is allowed.
    let _lock = if cfg.dry_run {
        None
    } else {
        let lock_path = output.join(".unloot.lock");
        match FileLock::acquire(&lock_path) {
            Ok(l) => Some(l),
            Err(e) => {
                return Err(e).with_context(|| {
                    format!(
                        "another unloot run already holds {} — wait for it, or point --output-dir somewhere else",
                        lock_path.display()
                    )
                });
            }
        }
    };

    let client: Box<dyn Llm> = if cfg.mock_llm {
        Box::new(llm::MockLlm::new(cfg)?)
    } else {
        // An empty --llm-model means "whatever the endpoint has loaded".
        let (client, _auto) = llm::HttpLlm::resolve(cfg)?;
        Box::new(client)
    };

    let mut state = State::load(&cfg.state_file, cfg.resume);
    if !state.is_empty() {
        info!(
            photos = state.len(),
            "ledger loaded; those photos will be skipped"
        );
    }
    if !cfg.keep_source {
        warn!(
            input = %input.display(),
            "keep-source is off: full-resolution originals are deleted once their compressed copies are verified"
        );
    }
    if !cfg.resume && !state.is_empty() {
        warn!(
            "resume is off but the ledger has entries; photos already processed may no longer be on disk"
        );
    }

    let started = chrono::Local::now();
    let mut rows: Vec<Row> = Vec::new();
    let mut claimed: HashSet<String> = HashSet::new();
    let mut report = Report::default();
    let mut csv_path: Option<PathBuf> = None;

    // Item folders that are already sorted — by hand, on another machine, or by
    // an earlier run whose ledger is gone — get described where they stand.
    // Nothing inside them is written to; the summary just gains the rows it
    // was missing.
    if cfg.describe_dirs {
        let known = known_names(cfg, &output);
        let described = process_item_dirs(cfg, client.as_ref(), &mut rows, &known)?;
        report.items += described;
        report.described = described;
    }

    loop {
        let found = scan::scan(&input)?;
        let skipped: Vec<&scan::SourceImage> =
            found.iter().filter(|img| state.seen(&img.sha256)).collect();
        if !skipped.is_empty() {
            // Name the folders they came from, so "nothing to do" is explainable.
            let from: BTreeSet<String> = skipped
                .iter()
                .filter_map(|i| state.get(&i.sha256).map(|p| p.name.clone()))
                .collect();
            debug!(photos = skipped.len(), items = %from.iter().take(6).cloned().collect::<Vec<_>>().join(", "), "skipping photos already in the ledger");
        }
        let unseen: Vec<SourceImage> = found
            .into_iter()
            .filter(|img| !state.seen(&img.sha256))
            .collect();
        let ready = if cfg.watch {
            settled(&unseen, cfg.settle, SystemTime::now())
        } else {
            unseen.clone()
        };
        let waiting = unseen.len() - ready.len();
        if waiting > 0 {
            debug!(
                waiting,
                settle = cfg.settle,
                "photos still being written; leaving them for the next pass"
            );
        }

        let in_bytes: u64 = ready.iter().map(|i| i.bytes).sum();
        if !ready.is_empty() {
            info!(photos = ready.len(), source = %crate::util::kb(in_bytes as usize), "processing");
            let batch = process_batch(
                cfg,
                client.as_ref(),
                &mut state,
                &ready,
                &mut rows,
                &mut claimed,
                &mut csv_path,
            )?;
            report.items += batch.items;
            report.photos += batch.photos;
            report.bytes_out += batch.bytes_out;
            if !cfg.dry_run {
                state.save()?;
            }
        }

        if !cfg.watch {
            break;
        }
        info!(interval = cfg.watch_interval, "watching for new photos");
        std::thread::sleep(Duration::from_secs(cfg.watch_interval as u64));
    }

    if cfg.dry_run {
        // Still print what the CSV would have said, to a path we do not create.
        let path = csvout::resolve_name(&cfg.csv_name, &csvout::timestamp(&started), &output);
        report.csv = Some(path.clone());
        info!(path = %path.display(), rows = rows.len(), "dry run: nothing written or deleted");
        print_preview(cfg, &rows, &path);
        return Ok(report);
    }

    if rows.is_empty() {
        info!("nothing to report; no summary written");
        state.save()?;
        return Ok(report);
    }

    // A resumed run keeps appending to the report it started, so one interrupted
    // session produces one file rather than a fragment.
    let path = match csv_path.clone().or_else(|| state.csv().map(PathBuf::from)) {
        Some(p) if p.exists() => p,
        _ => csvout::resolve_name(&cfg.csv_name, &csvout::timestamp(&started), &output),
    };

    // A resumed run covers the whole folder, not just the photos that arrived
    // since: rows recorded by earlier runs come first, this batch after them.
    let mut all: Vec<Row> = state
        .prior_rows()
        .iter()
        .map(|f| Row::from_fields(f, cfg))
        .filter(|r| !rows.iter().any(|n| n.name == r.name))
        .collect();
    all.extend(rows.iter().cloned());

    csvout::write(&path, cfg, &all)?;
    info!(path = %path.display(), rows = all.len(), "summary written");
    // Read it back: a summary that lost a row in the writing is worse than no
    // summary, and this is the cheap way to know.
    let back = csvout::read_rows(&path, cfg).context("read the summary back to check it")?;
    if back.len() != all.len() {
        bail!(
            "summary {} has {} rows, we wrote {}",
            path.display(),
            back.len(),
            all.len()
        );
    }
    state.remember_csv(&path);
    state.save()?;
    report.csv = Some(path);
    Ok(report)
}

struct Batch {
    items: usize,
    photos: usize,
    bytes_out: usize,
}

fn process_batch(
    cfg: &Config,
    client: &dyn Llm,
    state: &mut State,
    images: &[SourceImage],
    rows: &mut Vec<Row>,
    claimed: &mut HashSet<String>,
    csv_path: &mut Option<PathBuf>,
) -> Result<Batch> {
    let rows_before = rows.len();
    let clusters = catalog::cluster(cfg, client, images)?;
    let items = catalog::catalogue(cfg, client, clusters)?;

    let mut batch = Batch {
        items: 0,
        photos: 0,
        bytes_out: 0,
    };

    if cfg.dry_run {
        // Encode to show the real numbers, and write nothing.
        for item in &items {
            let mut bytes = 0usize;
            let mut dims = (0u32, 0u32);
            let mut wrote = false;
            for img in &item.images {
                if state.seen(&img.sha256) {
                    continue;
                }
                let enc = imageops::encode_limited(
                    &img.path,
                    cfg.max_long_edge,
                    cfg.max_bytes,
                    cfg.jpeg_quality_min,
                    cfg.jpeg_quality_max,
                )
                .with_context(|| format!("would encode {}", img.path.display()))?;
                bytes += enc.bytes.len();
                dims = bigger(dims, (enc.width, enc.height));
                wrote = true;
            }
            if !wrote {
                continue;
            }
            let mut row = row_for(cfg, item, dims, bytes, item.images.len());
            row.name = claim_name(cfg, claimed, &item.name);
            batch.photos += item.images.len();
            batch.bytes_out += bytes;
            rows.push(row);
        }
        batch.items = rows.len() - rows_before;
        return Ok(batch);
    }

    for item in items {
        let name = claim_name(cfg, claimed, &item.name);
        if name != item.name {
            warn!(from = %item.name, to = %name, "name already taken; using a numbered name");
        }
        let dir = cfg.output_dir.join(&name);
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("create item directory {}", dir.display()))?;

        let pending: Vec<&SourceImage> = item
            .images
            .iter()
            .filter(|i| !state.seen(&i.sha256))
            .collect();
        let mut written: Vec<(String, usize)> = Vec::new();
        let mut dims = (0u32, 0u32);

        // Numbering continues from what is already there, so a resumed item
        // does not overwrite the photos it already saved.
        let start_at = next_index(&dir);
        for (index, img) in (start_at..).zip(pending.iter()) {
            let enc = imageops::encode_limited(
                &img.path,
                cfg.max_long_edge,
                cfg.max_bytes,
                cfg.jpeg_quality_min,
                cfg.jpeg_quality_max,
            )
            .with_context(|| format!("encode {}", img.path.display()))?;
            let dest = dir.join(format!("photo_{index:02}.jpg"));
            write_verified(&enc, &dest).with_context(|| format!("save {}", dest.display()))?;
            written.push((img.name.clone(), enc.bytes.len()));
            dims = bigger(dims, (enc.width, enc.height));
            batch.photos += 1;
            batch.bytes_out += enc.bytes.len();
        }

        // Only now, with every copy checked, may the originals go.
        if !cfg.keep_source {
            for img in &pending {
                match std::fs::remove_file(&img.path) {
                    Ok(()) => debug!(path = %img.path.display(), "source removed"),
                    Err(e) => {
                        warn!(path = %img.path.display(), error = %e, "could not remove source")
                    }
                }
            }
        }

        let on_disk = total_bytes(&dir);
        debug!(item = %name, photos = written.len(), bytes = on_disk, "item written");
        let row = row_for(cfg, &item, dims, on_disk, count_photos(&dir));

        let name_for_state = row.name.clone();
        let fields = row.fields(cfg);
        for img in &pending {
            state.record(Processed {
                sha256: img.sha256.clone(),
                name: name_for_state.clone(),
                row: fields.clone(),
            });
        }
        batch.items += 1;
        rows.push(row);
    }

    if !rows.is_empty() && csv_path.is_none() {
        *csv_path = Some(csvout::resolve_name(
            &cfg.csv_name,
            &csvout::timestamp(&chrono::Local::now()),
            &cfg.output_dir,
        ));
    }
    if let Some(path) = csv_path.clone() {
        csvout::write(&path, cfg, rows)?;
        state.remember_csv(&path);
    }
    Ok(batch)
}

/// Write to a temporary name in the same directory, read it back, then rename.
fn write_verified(enc: &imageops::Encoded, dest: &Path) -> Result<()> {
    let tmp = dest.with_file_name(format!(
        ".{}.tmp",
        dest.file_name().and_then(|s| s.to_str()).unwrap_or("photo")
    ));
    std::fs::write(&tmp, &enc.bytes).with_context(|| format!("write {}", tmp.display()))?;

    let size = std::fs::metadata(&tmp)
        .map(|m| m.len())
        .context("stat temp photo")?;
    if size != enc.bytes.len() as u64 {
        std::fs::remove_file(&tmp).ok();
        bail!("wrote {size} bytes, expected {}", enc.bytes.len());
    }
    let (w, h) = imageops::dimensions(&tmp).context("verify temp photo decodes")?;
    if (w, h) != (enc.width, enc.height) {
        std::fs::remove_file(&tmp).ok();
        bail!(
            "saved photo decodes as {w}x{h}, expected {}x{}",
            enc.width,
            enc.height
        );
    }
    std::fs::rename(&tmp, dest).with_context(|| format!("rename into {}", dest.display()))?;
    Ok(())
}

/// Which items the summary already lists, so a folder is described once and
/// not added again on every subsequent run.
fn known_names(cfg: &Config, output: &Path) -> HashSet<String> {
    let mut names: HashSet<String> = HashSet::new();
    // The summary already in the folder is the record of what has been listed
    // before — not the ledger, which tracks photos it processed. Delete the
    // summary and the folders are described again; that is what deleting it means.
    if let Some(path) = csvout::latest_summary(cfg, output) {
        match csvout::read_rows(&path, cfg) {
            Ok(existing) => {
                debug!(path = %path.display(), rows = existing.len(), "reading the summary already here");
                names.extend(existing.into_iter().map(|r| r.name));
            }
            Err(e) => warn!(
                path = %path.display(),
                error = %format!("{e:#}"),
                "could not read the existing summary; every folder looks new"
            ),
        }
    }
    names
}

/// Describe the item folders already sitting in the output directory.
///
/// Their names are taken as given, their photos are only read, and a folder
/// that will not describe itself is skipped rather than fatal — the rest of the
/// listing is still worth having.
fn process_item_dirs(
    cfg: &Config,
    client: &dyn Llm,
    rows: &mut Vec<Row>,
    known: &HashSet<String>,
) -> Result<usize> {
    let dirs = scan::scan_item_dirs(&cfg.output_dir)?;
    let total = dirs.len();
    let fresh: Vec<scan::ItemDir> = dirs
        .into_iter()
        .filter(|d| !known.contains(&d.name))
        .collect();
    if total > fresh.len() {
        debug!(
            listed = total - fresh.len(),
            "folders already in the summary are left alone"
        );
    }
    if fresh.is_empty() {
        return Ok(0);
    }
    info!(
        folders = fresh.len(),
        "describing folders that are already sorted; their photos are read, not changed"
    );
    let mut described = 0;
    for result in catalog::describe_folders(cfg, client, fresh) {
        match result {
            Ok((dir, meta)) => {
                debug!(folder = %dir.path.display(), meta = %meta, "folder described");
                rows.push(row_for_folder(cfg, &dir, &meta));
                described += 1;
            }
            Err(e) if cfg.best_effort => {
                warn!(error = %format!("{e:#}"), "could not describe this folder; it stays out of the summary")
            }
            Err(e) => return Err(e),
        }
    }
    Ok(described)
}

/// The row for a folder: its own name and the model's attributes. The size and
/// byte columns come from what is on disk, for the dry run to print.
fn row_for_folder(cfg: &Config, dir: &scan::ItemDir, meta: &Meta) -> Row {
    let item = Item {
        name: dir.name.clone(),
        meta: meta.clone(),
        images: dir.images.clone(),
    };
    let mut dims = (0u32, 0u32);
    for img in &dir.images {
        if let Ok(d) = imageops::dimensions(&img.path) {
            dims = bigger(dims, d);
        }
    }
    let bytes = dir.images.iter().map(|i| i.bytes as usize).sum();
    row_for(cfg, &item, dims, bytes, dir.images.len())
}

/// The CSV row for one item. `dims` is the widest and tallest output photo in
/// the item — photos of one item can differ if the phone rotated some.
fn row_for(cfg: &Config, item: &Item, dims: (u32, u32), bytes: usize, photo_count: usize) -> Row {
    let meta = &item.meta;
    let na = |s: &str| s.is_empty();
    Row {
        name: item.name.clone(),
        category: if na(&meta.category) {
            cfg.na_label.clone()
        } else {
            meta.category.clone()
        },
        brand: meta
            .brand
            .clone()
            .filter(|b| !b.is_empty())
            .unwrap_or_else(|| cfg.unbranded.clone()),
        color: if na(&meta.color) {
            cfg.na_label.clone().to_lowercase()
        } else {
            meta.color.clone()
        },
        size: meta
            .size
            .clone()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| cfg.na_label.clone()),
        // Whole units only. An estimate to the cent implies a precision nobody
        // has, and second-hand buyers round anyway.
        price: format!("{}", meta.price.max(0.0).round()),
        weight_oz: format!("{:.1}", meta.weight_g / G_PER_OZ),
        weight_g: format!("{:.0}", meta.weight_g),
        photos: photo_count,
        source_files: item
            .images
            .iter()
            .map(|i| i.name.clone())
            .collect::<Vec<_>>()
            .join(", "),
        width: dims.0,
        height: dims.1,
        bytes,
    }
}

fn bigger(a: (u32, u32), b: (u32, u32)) -> (u32, u32) {
    (a.0.max(b.0), a.1.max(b.1))
}

fn count_photos(dir: &Path) -> usize {
    photo_names_in(dir).len()
}

fn total_bytes(dir: &Path) -> usize {
    photo_names_in(dir)
        .into_iter()
        .filter_map(|n| std::fs::metadata(dir.join(n)).ok())
        .map(|m| m.len() as usize)
        .sum()
}

/// Pick a free directory name and claim it, so two items in one batch never
/// land in the same folder. "Taken" means claimed by this run or already on
/// disk — including a folder a previous run made for the same item.
fn claim_name(cfg: &Config, claimed: &mut HashSet<String>, base: &str) -> String {
    let taken =
        |candidate: &str| claimed.contains(candidate) || cfg.output_dir.join(candidate).exists();
    let name = catalog::unique_name(&taken, base);
    claimed.insert(name.clone());
    name
}

fn photo_names_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter_map(|e| e.file_name().to_str().map(str::to_string))
                .filter(|n| n.starts_with("photo_") && n.ends_with(".jpg"))
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

fn next_index(dir: &Path) -> usize {
    photo_names_in(dir)
        .iter()
        .filter_map(|n| {
            n.strip_prefix("photo_")?
                .strip_suffix(".jpg")?
                .parse::<usize>()
                .ok()
        })
        .max()
        .map(|m| m + 1)
        .unwrap_or(1)
}

fn print_preview(cfg: &Config, rows: &[Row], csv_path: &Path) {
    println!("dry run — nothing will be written or deleted");
    println!("input : {}", cfg.input_dir.display());
    println!(
        "output: {}{}",
        cfg.output_dir.display(),
        if cfg.in_place() { "  (in place)" } else { "" }
    );
    println!("report: {}", csv_path.display());
    println!("items : {}", rows.len());
    println!();
    for row in rows {
        println!(
            "  {name}/  [{n} photo{s}: {w}x{h}, {kb}]  {cat} · {brand} · {color} · size {size} · {cur} {price} · {oz} oz ({g} g)",
            name = row.name,
            n = row.photos,
            s = if row.photos == 1 { "" } else { "s" },
            w = row.width,
            h = row.height,
            kb = util::kb(row.bytes),
            cat = row.category,
            brand = row.brand,
            color = row.color,
            size = row.size,
            cur = cfg.currency,
            price = row.price,
            oz = row.weight_oz,
            g = row.weight_g,
        );
        println!("    from: {}", row.source_files);
    }
}
