//! Configuration, stored as JSON next to the download database.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub const DEFAULT_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
(KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36";

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Config {
    pub download_dir: String,
    pub connections: u32,
    pub max_concurrent: u32,
    /// bytes per second, 0 = unlimited
    pub speed_limit: u64,
    pub user_agent: String,
    pub proxy: Option<String>,
    pub retries: u32,
    pub timeout_secs: u64,
    /// save into per-category subfolders (Video, Music, Programs, ...)
    pub categories: bool,
    pub api_port: u16,
    /// user-resized column widths for the download table
    #[serde(default)]
    pub column_widths: Option<[f32; 8]>,
}

impl Default for Config {
    fn default() -> Self {
        let download_dir = dirs::download_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("TDM")
            .to_string_lossy()
            .to_string();
        Self {
            download_dir,
            connections: 8,
            max_concurrent: 4,
            speed_limit: 0,
            user_agent: DEFAULT_UA.to_string(),
            proxy: None,
            retries: 5,
            timeout_secs: 60,
            categories: false,
            api_port: 8484,
            column_widths: None,
        }
    }
}

impl Config {
    pub fn home() -> PathBuf {
        if let Ok(dir) = std::env::var("TDM_HOME") {
            return PathBuf::from(dir);
        }
        dirs::data_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("tdm")
    }

    pub fn path() -> PathBuf {
        Self::home().join("config.json")
    }

    pub fn load() -> Self {
        match std::fs::read_to_string(Self::path()) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let path = Self::path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(self)?)?;
        // Windows rename does not replace an existing file
        if path.exists() {
            std::fs::remove_file(&path)?;
        }
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    /// Apply a `key=value` override, used by `tdm config set`.
    pub fn set(&mut self, key: &str, value: &str) -> anyhow::Result<()> {
        match key {
            "download_dir" => self.download_dir = value.to_string(),
            "connections" => self.connections = value.parse()?,
            "max_concurrent" => self.max_concurrent = value.parse()?,
            "speed_limit" => self.speed_limit = crate::util::parse_size(value).unwrap_or(0),
            "user_agent" => self.user_agent = value.to_string(),
            "proxy" => {
                self.proxy = if value.is_empty() || value == "none" {
                    None
                } else {
                    Some(value.to_string())
                }
            }
            "retries" => self.retries = value.parse()?,
            "timeout_secs" => self.timeout_secs = value.parse()?,
            "categories" => self.categories = matches!(value, "1" | "true" | "yes" | "on"),
            "api_port" => self.api_port = value.parse()?,
            other => anyhow::bail!("unknown config key: {}", other),
        }
        Ok(())
    }
}
