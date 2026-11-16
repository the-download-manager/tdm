//! Small helpers: formatting, filename hygiene, hashing.

use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Shorten a string for table/bar display.
pub fn ellipsize(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let keep = max.saturating_sub(3);
    let mut out: String = s.chars().take(keep).collect();
    out.push_str("...");
    out
}

pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    if n < 1024 {
        return format!("{} B", n);
    }
    let mut value = n as f64;
    let mut idx = 0usize;
    while value >= 1024.0 && idx < UNITS.len() - 1 {
        value /= 1024.0;
        idx += 1;
    }
    format!("{:.2} {}", value, UNITS[idx])
}

pub fn human_speed(bps: u64) -> String {
    format!("{}/s", human_bytes(bps))
}

pub fn human_duration(secs: u64) -> String {
    if secs < 60 {
        format!("{}s", secs)
    } else if secs < 3600 {
        format!("{}m {:02}s", secs / 60, secs % 60)
    } else {
        format!("{}h {:02}m", secs / 3600, (secs % 3600) / 60)
    }
}

/// Parse "10M", "500k", "2G", "1048576" into bytes per second.
pub fn parse_size(text: &str) -> Option<u64> {
    let t = text.trim().to_ascii_lowercase();
    if t.is_empty() {
        return None;
    }
    let (num, mult) = match t.chars().last()? {
        'k' => (&t[..t.len() - 1], 1024u64),
        'm' => (&t[..t.len() - 1], 1024 * 1024),
        'g' => (&t[..t.len() - 1], 1024 * 1024 * 1024),
        _ => (t.as_str(), 1),
    };
    let value: f64 = num.trim().parse().ok()?;
    if value < 0.0 {
        return None;
    }
    Some((value * mult as f64) as u64)
}

/// Remove characters that are illegal in Windows file names and trim the result.
pub fn sanitize_filename(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        let bad = c.is_control()
            || matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*');
        out.push(if bad { '_' } else { c });
    }
    // Windows reserves trailing dots/spaces
    while out.ends_with(' ') || out.ends_with('.') {
        out.pop();
    }
    let trimmed = out.trim_start_matches(' ');
    if trimmed.is_empty() {
        return "download.bin".to_string();
    }
    let mut s = trimmed.to_string();
    if s.chars().count() > 180 {
        // keep the extension when truncating
        let ext = Path::new(&s)
            .extension()
            .map(|e| format!(".{}", e.to_string_lossy()))
            .unwrap_or_default();
        let keep = 180usize.saturating_sub(ext.chars().count());
        s = s.chars().take(keep).collect::<String>() + &ext;
    }
    // reserved device names
    let stem = s.split('.').next().unwrap_or("").to_ascii_uppercase();
    if matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4 && (stem.starts_with("COM") || stem.starts_with("LPT")))
    {
        s = format!("_{}", s);
    }
    s
}

/// Return a path that does not exist yet, appending " (n)" before the extension.
pub fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let candidate = dir.join(name);
    if !candidate.exists() {
        return candidate;
    }
    let path = Path::new(name);
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "download".into());
    let ext = path
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    for i in 1..10_000 {
        let candidate = dir.join(format!("{} ({}){}", stem, i, ext));
        if !candidate.exists() {
            return candidate;
        }
    }
    dir.join(format!("{}-{}{}", stem, now_unix(), ext))
}

/// Streaming SHA-256 of a file (blocking; call from spawn_blocking for large files).
pub fn sha256_file(path: &Path) -> std::io::Result<String> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// Best-effort category from a file extension.
pub fn category_for(name: &str) -> &'static str {
    let ext = Path::new(name)
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "zip" | "rar" | "7z" | "tar" | "gz" | "bz2" | "xz" | "zst" | "iso" => "Compressed",
        "pdf" => "Documents",
        "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx" | "txt" | "rtf" | "epub" => "Documents",
        "mp3" | "wav" | "flac" | "aac" | "ogg" | "m4a" | "wma" => "Music",
        "mp4" | "mkv" | "avi" | "mov" | "wmv" | "flv" | "webm" | "mpg" | "m4v" => "Video",
        "jpg" | "jpeg" | "png" | "gif" | "bmp" | "webp" | "svg" | "ico" => "Images",
        "exe" | "msi" | "dll" | "bin" | "apk" | "dmg" | "pkg" | "deb" | "rpm" => "Programs",
        _ => "Other",
    }
}
