//! Download database: one JSON file per download under `<home>/downloads/<id>.json`.
//!
//! A file-per-download layout keeps concurrent workers from fighting over a single
//! index file, and makes each progress checkpoint independent.

use crate::config::Config;
use crate::model::Download;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct Store {
    pub base: PathBuf,
}

impl Store {
    pub fn new() -> Self {
        Self { base: Config::home() }
    }

    pub fn at(base: PathBuf) -> Self {
        Self { base }
    }

    pub fn downloads_dir(&self) -> PathBuf {
        self.base.join("downloads")
    }

    fn record_path(&self, id: &str) -> PathBuf {
        self.downloads_dir().join(format!("{}.json", id))
    }

    pub fn ensure(&self) -> Result<()> {
        std::fs::create_dir_all(self.downloads_dir())
            .with_context(|| format!("creating {}", self.downloads_dir().display()))?;
        Ok(())
    }

    /// Atomic-ish save: write a temp file, then replace the record.
    pub fn save(&self, dl: &Download) -> Result<()> {
        self.ensure()?;
        let path = self.record_path(&dl.id);
        let tmp = path.with_extension("json.tmp");
        let text = serde_json::to_string_pretty(dl)?;
        std::fs::write(&tmp, text)?;
        if path.exists() {
            std::fs::remove_file(&path)?;
        }
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    pub fn load(&self, id: &str) -> Option<Download> {
        let path = self.record_path(id);
        let text = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&text).ok()
    }

    pub fn load_all(&self) -> Vec<Download> {
        let dir = self.downloads_dir();
        let mut items = Vec::new();
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return items;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            if let Ok(text) = std::fs::read_to_string(&path) {
                if let Ok(dl) = serde_json::from_str::<Download>(&text) {
                    items.push(dl);
                }
            }
        }
        items.sort_by_key(|d| d.created_at);
        items
    }

    pub fn delete(&self, id: &str) -> Result<()> {
        let path = self.record_path(id);
        if path.exists() {
            std::fs::remove_file(path)?;
        }
        Ok(())
    }

    /// Resolve a short id (or full id) to a stored download.
    pub fn resolve(&self, id: &str) -> Option<Download> {
        if let Some(dl) = self.load(id) {
            return Some(dl);
        }
        self.load_all()
            .into_iter()
            .find(|d| d.id.starts_with(id) || d.short_id() == id)
    }

    pub fn data_path(&self, dl: &Download) -> PathBuf {
        Path::new(&dl.dir).join(&dl.file_name)
    }
}

impl Default for Store {
    fn default() -> Self {
        Self::new()
    }
}
