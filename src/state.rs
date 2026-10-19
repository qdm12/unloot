//! What this folder has already been told about.
//!
//! The JSON next to the output is how a re-run knows it has seen a photo
//! before, which matters because the default behaviour deletes the source: a
//! half-finished run must be able to finish without re-reading, re-paying, or
//! double-listing anything.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

pub const VERSION: u32 = 1;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Processed {
    pub sha256: String,
    /// Directory the photos ended up in.
    pub name: String,
    /// The CSV row, already stringified, so a resumed run can reprint it.
    pub row: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Persisted {
    pub version: u32,
    #[serde(default)]
    pub csv: Option<PathBuf>,
    #[serde(default)]
    pub items: Vec<Processed>,
}

pub struct State {
    path: PathBuf,
    items: HashMap<String, Processed>,
    csv: Option<PathBuf>,
    enabled: bool,
}

impl State {
    /// Read the state file. Without `resume` we start empty — but note the
    /// caller decides what to do about the sources that are no longer there.
    pub fn load(path: &Path, enabled: bool) -> Self {
        let mut state = State {
            path: path.to_path_buf(),
            items: HashMap::new(),
            csv: None,
            enabled,
        };
        if !enabled {
            return state;
        }
        match fs::read_to_string(path) {
            Ok(text) => match serde_json::from_str::<Persisted>(&text) {
                Ok(p) => {
                    if p.version != VERSION {
                        warn!(path = %path.display(), found = p.version, expected = VERSION, "state file is from another version; ignoring it");
                        return state;
                    }
                    state.csv = p.csv.clone();
                    for item in p.items {
                        state.items.insert(item.sha256.clone(), item);
                    }
                    info!(path = %path.display(), photos = state.items.len(), "resuming");
                }
                Err(e) => {
                    warn!(path = %path.display(), error = %e, "could not parse state file; starting fresh")
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                warn!(path = %path.display(), error = %e, "could not read state file; starting fresh")
            }
        }
        state
    }

    pub fn get(&self, sha256: &str) -> Option<&Processed> {
        self.items.get(sha256)
    }

    /// Rows recorded by earlier runs, one per item name, name-ordered.
    ///
    /// A resumed run prints a listing of the whole folder, not just the photos
    /// that arrived since — the row was kept for exactly this.
    pub fn prior_rows(&self) -> Vec<Vec<String>> {
        let mut by_name: std::collections::BTreeMap<&str, &Vec<String>> =
            std::collections::BTreeMap::new();
        for p in self.items.values() {
            if !p.row.is_empty() {
                by_name.entry(p.name.as_str()).or_insert(&p.row);
            }
        }
        by_name.into_values().cloned().collect()
    }

    pub fn seen(&self, sha256: &str) -> bool {
        self.items.contains_key(sha256)
    }

    pub fn record(&mut self, processed: Processed) {
        self.items.insert(processed.sha256.clone(), processed);
    }

    pub fn csv(&self) -> Option<&Path> {
        self.csv.as_deref()
    }

    pub fn remember_csv(&mut self, path: &Path) {
        self.csv = Some(path.to_path_buf());
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Write it out. A state file we cannot write means the next run will not
    /// know what it has done, so this is worth a warning, not a failure: the
    /// photos on disk are the truth, the state file is a convenience.
    pub fn save(&self) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }
        let mut items: Vec<&Processed> = self.items.values().collect();
        items.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.sha256.cmp(&b.sha256)));
        let persisted = Persisted {
            version: VERSION,
            csv: self.csv.clone(),
            items: items.into_iter().cloned().collect(),
        };
        let text = serde_json::to_string_pretty(&persisted).context("serialize state")?;
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
        }
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, text.as_bytes())
            .with_context(|| format!("write {}", tmp.display()))?;
        std::fs::rename(&tmp, &self.path)
            .with_context(|| format!("rename state into {}", self.path.display()))?;
        debug!(path = %self.path.display(), photos = self.items.len(), "state saved");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_resume() {
        let dir = std::env::temp_dir().join(format!("unloot-state-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.json");

        {
            let mut s = State::load(&path, true);
            assert!(s.is_empty());
            s.record(Processed {
                sha256: "abc".into(),
                name: "Bag black".into(),
                row: vec!["Bag black".into(), "Bag".into()],
            });
            s.remember_csv(Path::new("summary_260215_101500.csv"));
            s.save().unwrap();
        }
        let s = State::load(&path, true);
        assert_eq!(s.len(), 1);
        assert!(s.seen("abc"));
        assert!(!s.seen("zzz"));
        assert_eq!(s.get("abc").unwrap().row[0], "Bag black");
        assert_eq!(
            s.csv().unwrap().to_str().unwrap(),
            "summary_260215_101500.csv"
        );

        // Disabled: nothing is remembered, and nothing is read back.
        let off = State::load(&path, false);
        assert!(off.is_empty());

        // A state file from another version is ignored rather than trusted.
        fs::write(
            &path,
            r#"{"version":99,"items":[{"sha256":"x","name":"n","row":[]}]}"#,
        )
        .unwrap();
        assert!(State::load(&path, true).is_empty());

        fs::remove_dir_all(&dir).ok();
    }
}
