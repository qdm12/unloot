//! unloot — a folder of phone photos in, per-item listing directories and a
//! CSV summary out.
//!
//! Run `unloot --help`, or `unloot --print-config` to see every option and
//! where its value came from. The pipeline itself lives in [`pipeline`].

mod catalog;
mod config;
mod csvout;
mod imageops;
mod llm;
mod pipeline;
mod scan;
mod state;
mod util;

use std::process::ExitCode;

use anyhow::{Context, Result};
use tracing::{error, info, warn};

fn main() -> ExitCode {
    let cfg = match config::resolve() {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("unloot: {e:#}");
            return ExitCode::from(2);
        }
    };

    util::init_tracing(&cfg.log);

    let result = run(cfg);
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            // One line per cause: the chain is the diagnosis, and a wall of
            // backtrace would hide it.
            error!("{e:#}");
            for cause in e.chain().skip(1) {
                error!("  caused by: {cause}");
            }
            eprintln!("unloot failed: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(cfg: config::Config) -> Result<()> {
    info!(version = env!("CARGO_PKG_VERSION"), "unloot starting");
    if cfg.dry_run {
        info!("dry run: no file is written or deleted");
    }

    let report = pipeline::run(&cfg).context("run failed")?;

    if cfg.dry_run {
        return Ok(());
    }
    let csv = report
        .csv
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "none".into());
    info!(
        items = report.items,
        photos = report.photos,
        bytes_out = report.bytes_out,
        csv = %csv,
        "done"
    );
    if report.items == 0 {
        warn!(
            "nothing to report — check --input-dir, the ledger, and whether the summary already lists these items"
        );
    }
    // Sorting loose photos and describing a sorted shelf are very different
    // amounts of work, so the closing line keeps them apart.
    let sorted = report.items.saturating_sub(report.described);
    let mut done = format!("{sorted} item(s) sorted from loose photos");
    if report.described > 0 {
        done = format!("{done}, {} described where they stand", report.described);
    }
    println!(
        "{done}: {} photo(s), {:.1} MiB written. Summary: {csv}",
        report.photos,
        report.bytes_out as f64 / 1_048_576.0
    );
    Ok(())
}
