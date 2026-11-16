//! Persisted download model: URL, target file,
//! connection list and the byte range each connection is responsible for.

use serde::{Deserialize, Serialize};

fn default_connections() -> u32 {
    8
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Queued,
    Connecting,
    Downloading,
    Paused,
    Completed,
    Error,
    Canceled,
}

impl Status {
    pub fn as_str(&self) -> &'static str {
        match self {
            Status::Queued => "queued",
            Status::Connecting => "connecting",
            Status::Downloading => "downloading",
            Status::Paused => "paused",
            Status::Completed => "completed",
            Status::Error => "error",
            Status::Canceled => "canceled",
        }
    }

    pub fn is_active(&self) -> bool {
        matches!(self, Status::Connecting | Status::Downloading)
    }

    pub fn is_finished(&self) -> bool {
        matches!(self, Status::Completed)
    }
}

/// Extra HTTP header supplied by the user (cookies, auth tokens, ...).
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Header {
    pub name: String,
    pub value: String,
}

/// One connection's slice of the file. `end` is exclusive.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Segment {
    pub start: u64,
    pub end: u64,
    pub done: u64,
}

impl Segment {
    pub fn len(&self) -> u64 {
        self.end.saturating_sub(self.start)
    }

    pub fn remaining(&self) -> u64 {
        self.len().saturating_sub(self.done)
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Download {
    pub id: String,
    pub url: String,
    #[serde(default)]
    pub final_url: Option<String>,
    pub file_name: String,
    pub dir: String,
    pub size: u64,
    #[serde(default)]
    pub downloaded: u64,
    pub status: Status,
    #[serde(default)]
    pub segments: Vec<Segment>,
    #[serde(default = "default_connections")]
    pub connections: u32,
    pub created_at: i64,
    pub updated_at: i64,
    #[serde(default)]
    pub etag: Option<String>,
    #[serde(default)]
    pub last_modified: Option<String>,
    #[serde(default)]
    pub content_type: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub category: String,
    #[serde(default)]
    pub referer: Option<String>,
    #[serde(default)]
    pub user_agent: Option<String>,
    #[serde(default)]
    pub cookie: Option<String>,
    #[serde(default)]
    pub checksum: Option<String>,
    /// true when the user supplied the file name, so probe results must not override it
    #[serde(default)]
    pub name_locked: bool,
    /// false when the server ignored Range requests (single-connection fallback)
    #[serde(default = "default_true")]
    pub ranges_supported: bool,
    /// arbitrary user headers
    #[serde(default)]
    pub headers: Vec<Header>,
}

fn default_true() -> bool {
    true
}

impl Download {
    pub fn new(url: String, dir: String, file_name: String, connections: u32) -> Self {
        let now = crate::util::now_unix();
        let category = crate::util::category_for(&file_name).to_string();
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            url,
            final_url: None,
            file_name,
            dir,
            size: 0,
            downloaded: 0,
            status: Status::Queued,
            segments: Vec::new(),
            connections: connections.max(1).min(64),
            created_at: now,
            updated_at: now,
            etag: None,
            last_modified: None,
            content_type: None,
            error: None,
            category,
            referer: None,
            user_agent: None,
            cookie: None,
            checksum: None,
            name_locked: false,
            ranges_supported: true,
            headers: Vec::new(),
        }
    }

    /// Path of the in-progress file.
    pub fn part_path(&self) -> std::path::PathBuf {
        std::path::Path::new(&self.dir).join(format!("{}.tdm-part", self.file_name))
    }

    pub fn final_path(&self) -> std::path::PathBuf {
        std::path::Path::new(&self.dir).join(&self.file_name)
    }

    pub fn progress_fraction(&self) -> f64 {
        if self.size == 0 {
            return 0.0;
        }
        (self.downloaded as f64 / self.size as f64).clamp(0.0, 1.0)
    }

    pub fn recompute_downloaded(&mut self) {
        self.downloaded = self.segments.iter().map(|s| s.done).sum();
    }

    /// Short id used on the command line.
    pub fn short_id(&self) -> String {
        self.id.chars().take(8).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segments_math() {
        let s = Segment { start: 100, end: 200, done: 30 };
        assert_eq!(s.len(), 100);
        assert_eq!(s.remaining(), 70);
    }

    #[test]
    fn progress_fraction_handles_zero_size() {
        let d = Download::new("http://x/y".into(), ".".into(), "y".into(), 4);
        assert_eq!(d.progress_fraction(), 0.0);
    }
}
