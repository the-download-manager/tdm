//! Video downloader: HLS (.m3u8) and DASH (.mpd) manifests.
//!
//! The pipeline used after a browser hands over a media URL:
//!   1. fetch and parse the manifest (master playlist / MPD),
//!   2. pick the best variant (and the separate audio track when there is one),
//!   3. download every segment concurrently, concatenate in order,
//!   4. remux/mux with ffmpeg (`-c copy`, no re-encoding).
//!
//! DRM-protected streams are not supported and encrypted HLS is rejected.

use crate::config::Config;
use crate::http::HttpClient;
use crate::util::{human_bytes, sanitize_filename};
use anyhow::{anyhow, bail, Context, Result};
use indicatif::{ProgressBar, ProgressStyle};
use roxmltree::{Document, Node};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use url::Url;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Quality {
    Best,
    Worst,
    Height(u32),
}

#[derive(Clone)]
pub struct VideoOptions {
    pub url: String,
    pub out: Option<PathBuf>,
    pub quality: Quality,
    pub audio_only: bool,
    pub list_only: bool,
    pub keep_temp: bool,
    pub concurrency: usize,
    pub referer: Option<String>,
    pub progress: Option<std::sync::Arc<crate::engine::Progress>>,
    /// persistent folder for segment parts (enables resume after a failure)
    pub work_dir: Option<PathBuf>,
}

impl Default for VideoOptions {
    fn default() -> Self {
        Self {
            url: String::new(),
            out: None,
            quality: Quality::Best,
            audio_only: false,
            list_only: false,
            keep_temp: false,
            concurrency: 6,
            referer: None,
            progress: None,
            work_dir: None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct SegmentRef {
    pub url: String,
    /// optional byte range (start, length) inside the URL
    pub range: Option<(u64, u64)>,
}

#[derive(Clone, Debug, Default)]
pub struct Variant {
    pub url: String,
    pub bandwidth: u64,
    pub width: u32,
    pub height: u32,
    pub codecs: String,
    pub audio_group: Option<String>,
}

#[derive(Clone, Debug)]
pub struct DashStream {
    pub id: String,
    pub bandwidth: u64,
    pub width: u32,
    pub height: u32,
    pub codecs: String,
    pub audio: bool,
    pub init: Option<SegmentRef>,
    pub segments: Vec<SegmentRef>,
}

// ---------------------------------------------------------------------------
// HLS
// ---------------------------------------------------------------------------

fn parse_attrs(input: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let mut key = String::new();
    let mut value = String::new();
    let mut in_key = true;
    let mut quoted = false;
    for c in input.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                if !quoted {
                    map.insert(key.trim().to_string(), value.trim().to_string());
                    key.clear();
                    value.clear();
                    in_key = true;
                }
            }
            '=' if in_key && !quoted => in_key = false,
            ',' if !quoted => {
                if !in_key {
                    map.insert(key.trim().to_string(), value.trim().to_string());
                    key.clear();
                    value.clear();
                    in_key = true;
                }
            }
            _ => {
                if in_key {
                    key.push(c);
                } else {
                    value.push(c);
                }
            }
        }
    }
    if !key.trim().is_empty() {
        map.insert(key.trim().to_string(), value.trim().to_string());
    }
    map
}

fn is_master(text: &str) -> bool {
    text.lines()
        .any(|l| l.trim_start().starts_with("#EXT-X-STREAM-INF"))
}

fn parse_master(text: &str, base: &Url) -> (Vec<Variant>, HashMap<String, String>) {
    let lines: Vec<&str> = text.lines().collect();
    let mut variants = Vec::new();
    let mut audio_groups: HashMap<String, String> = HashMap::new();
    let mut pending: Option<HashMap<String, String>> = None;

    for line in lines {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("#EXT-X-MEDIA:") {
            let attrs = parse_attrs(rest);
            if attrs.get("TYPE").map(|s| s.as_str()) == Some("AUDIO") {
                if let (Some(group), Some(uri)) = (attrs.get("GROUP-ID"), attrs.get("URI")) {
                    if let Ok(abs) = base.join(uri) {
                        audio_groups.insert(group.clone(), abs.to_string());
                    }
                }
            }
        } else if let Some(rest) = line.strip_prefix("#EXT-X-STREAM-INF:") {
            pending = Some(parse_attrs(rest));
        } else if !line.is_empty() && !line.starts_with('#') {
            if let Some(attrs) = pending.take() {
                let url = base.join(line).map(|u| u.to_string()).unwrap_or_default();
                variants.push(Variant {
                    url,
                    bandwidth: attrs.get("BANDWIDTH").and_then(|v| v.parse().ok()).unwrap_or(0),
                    width: attrs
                        .get("RESOLUTION")
                        .and_then(|r| r.split_once('x'))
                        .and_then(|(w, _)| w.parse().ok())
                        .unwrap_or(0),
                    height: attrs
                        .get("RESOLUTION")
                        .and_then(|r| r.split_once('x'))
                        .and_then(|(_, h)| h.parse().ok())
                        .unwrap_or(0),
                    codecs: attrs.get("CODECS").cloned().unwrap_or_default(),
                    audio_group: attrs.get("AUDIO").cloned(),
                });
            }
        }
    }
    (variants, audio_groups)
}

struct HlsMedia {
    segments: Vec<SegmentRef>,
    init: Option<SegmentRef>,
    encrypted: bool,
    ended: bool,
}

fn parse_media_playlist(text: &str, base: &Url) -> Result<HlsMedia> {
    let lines: Vec<&str> = text.lines().collect();
    let mut segments = Vec::new();
    let mut init = None;
    let mut encrypted = false;
    let mut ended = false;
    let mut pending_range: Option<(u64, u64)> = None;
    let mut last_end_by_url: HashMap<String, u64> = HashMap::new();

    for line in lines {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("#EXT-X-MAP:") {
            let attrs = parse_attrs(rest);
            if let Some(uri) = attrs.get("URI") {
                let url = base.join(uri)?.to_string();
                let range = attrs.get("BYTERANGE").and_then(|r| parse_byterange(r, 0));
                init = Some(SegmentRef { url, range });
            }
        } else if let Some(rest) = line.strip_prefix("#EXT-X-KEY:") {
            let attrs = parse_attrs(rest);
            if attrs.get("METHOD").map(|m| m.as_str()) != Some("NONE") {
                encrypted = true;
            }
        } else if let Some(rest) = line.strip_prefix("#EXT-X-BYTERANGE:") {
            let prev = segments.last().map(|s: &SegmentRef| s.url.clone());
            let default_off = prev
                .as_ref()
                .and_then(|u| last_end_by_url.get(u).copied())
                .unwrap_or(0);
            pending_range = parse_byterange(rest, default_off);
        } else if line == "#EXT-X-ENDLIST" {
            ended = true;
        } else if !line.is_empty() && !line.starts_with('#') {
            let url = base.join(line)?.to_string();
            if let Some((start, len)) = pending_range.take() {
                last_end_by_url.insert(url.clone(), start + len);
                segments.push(SegmentRef { url, range: Some((start, len)) });
            } else {
                segments.push(SegmentRef { url, range: None });
            }
        }
    }
    Ok(HlsMedia { segments, init, encrypted, ended })
}

/// `length[@offset]`
fn parse_byterange(value: &str, default_offset: u64) -> Option<(u64, u64)> {
    let (len, off) = match value.split_once('@') {
        Some((l, o)) => (l.trim().parse().ok()?, o.trim().parse().ok()?),
        None => (value.trim().parse().ok()?, default_offset),
    };
    Some((off, len))
}

// ---------------------------------------------------------------------------
// DASH
// ---------------------------------------------------------------------------

fn parse_iso_duration(value: &str) -> Option<f64> {
    let mut rest = value.trim();
    if let Some(stripped) = rest.strip_prefix("PT") {
        rest = stripped;
    } else if let Some(stripped) = rest.strip_prefix("P") {
        rest = stripped;
    }
    let mut secs = 0.0f64;
    let mut num = String::new();
    for c in rest.chars() {
        match c {
            '0'..='9' | '.' => num.push(c),
            'H' => {
                secs += num.parse::<f64>().ok()? * 3600.0;
                num.clear();
            }
            'M' => {
                secs += num.parse::<f64>().ok()? * 60.0;
                num.clear();
            }
            'S' => {
                secs += num.parse::<f64>().ok()?;
                num.clear();
            }
            _ => return None,
        }
    }
    Some(secs)
}

fn tag<'a, 'i>(node: &Node<'a, 'i>, name: &str) -> bool {
    node.is_element() && node.tag_name().name() == name
}

fn child<'a, 'i>(node: Node<'a, 'i>, name: &str) -> Option<Node<'a, 'i>> {
    node.children().find(|n| tag(n, name))
}

fn child_text<'a, 'i>(node: Node<'a, 'i>, name: &str) -> Option<String> {
    child(node, name)
        .and_then(|n| n.text())
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
}

fn format_number(value: u64, fmt: Option<&str>) -> String {
    if let Some(fmt) = fmt {
        let width = fmt
            .trim_start_matches('%')
            .trim_end_matches('d')
            .parse::<usize>()
            .unwrap_or(0);
        if width > 0 {
            return format!("{:0width$}", value, width = width);
        }
    }
    value.to_string()
}

fn substitute(template: &str, rep_id: &str, bandwidth: u64, number: u64, time: u64) -> String {
    let mut out = String::new();
    let mut rest = template;
    while let Some(start) = rest.find('$') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        if let Some(end) = after.find('$') {
            let key = &after[..end];
            let (name, fmt) = match key.find('%') {
                Some(p) => (&key[..p], Some(&key[p..])),
                None => (key, None),
            };
            let value = match name {
                "RepresentationID" => rep_id.to_string(),
                "Bandwidth" => bandwidth.to_string(),
                "Number" => format_number(number, fmt),
                "Time" => format_number(time, fmt),
                _ => format!("${}$", key),
            };
            out.push_str(&value);
            rest = &after[end + 1..];
        } else {
            out.push('$');
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

fn resolve(base: &Url, name: &str) -> Result<String> {
    Ok(base.join(name)?.to_string())
}

fn build_dash_segments(
    base: &Url,
    rep_id: &str,
    bandwidth: u64,
    tmpl: Option<Node>,
    seg_list: Option<Node>,
    period_duration: f64,
) -> Result<(Option<SegmentRef>, Vec<SegmentRef>)> {
    if let Some(tmpl) = tmpl {
        let media = tmpl
            .attribute("media")
            .context("SegmentTemplate without @media")?;
        let start_number = tmpl
            .attribute("startNumber")
            .and_then(|v| v.parse().ok())
            .unwrap_or(1);
        let timescale = tmpl
            .attribute("timescale")
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(1)
            .max(1);

        let init = tmpl
            .attribute("initialization")
            .map(|i| resolve(base, &substitute(i, rep_id, bandwidth, start_number, 0)))
            .transpose()?
            .map(|url| SegmentRef { url, range: None });

        let mut segments = Vec::new();
        if let Some(timeline) = child(tmpl, "SegmentTimeline") {
            let mut time: u64 = 0;
            let mut number = start_number;
            for s in timeline.children().filter(|n| tag(n, "S")) {
                if let Some(t) = s.attribute("t").and_then(|v| v.parse::<u64>().ok()) {
                    time = t;
                }
                let d = s
                    .attribute("d")
                    .and_then(|v| v.parse::<u64>().ok())
                    .context("SegmentTimeline <S> without @d")?;
                let repeat = s
                    .attribute("r")
                    .and_then(|v| v.parse::<i64>().ok())
                    .unwrap_or(0);
                let count = (repeat.max(0) + 1) as usize;
                for _ in 0..count {
                    let url = resolve(base, &substitute(media, rep_id, bandwidth, number, time))?;
                    segments.push(SegmentRef { url, range: None });
                    time += d;
                    number += 1;
                }
            }
        } else {
            let seg_duration = tmpl
                .attribute("duration")
                .and_then(|v| v.parse::<u64>().ok())
                .context("SegmentTemplate needs @duration or SegmentTimeline")?;
            if period_duration <= 0.0 {
                bail!("MPD has no duration; cannot enumerate segments");
            }
            let total = (period_duration * timescale as f64 / seg_duration as f64).ceil() as u64;
            for number in start_number..start_number + total {
                let url = resolve(base, &substitute(media, rep_id, bandwidth, number, 0))?;
                segments.push(SegmentRef { url, range: None });
            }
        }
        return Ok((init, segments));
    }

    if let Some(list) = seg_list {
        let init = list
            .attribute("initialization")
            .map(|i| resolve(base, i))
            .transpose()?
            .map(|url| SegmentRef { url, range: None });
        let mut segments = Vec::new();
        for seg in list.children().filter(|n| tag(n, "SegmentURL")) {
            if let Some(media) = seg.attribute("media") {
                segments.push(SegmentRef { url: resolve(base, media)?, range: None });
            }
        }
        return Ok((init, segments));
    }

    bail!("representation uses unsupported addressing (no SegmentTemplate/SegmentList)")
}

fn parse_mpd(text: &str, base: &Url) -> Result<Vec<DashStream>> {
    let doc = Document::parse(text).context("invalid MPD XML")?;
    let mpd = doc.root_element();
    let mpd_duration = mpd
        .attribute("mediaPresentationDuration")
        .and_then(parse_iso_duration)
        .unwrap_or(0.0);

    let mut streams = Vec::new();
    for period in mpd.children().filter(|n| tag(n, "Period")) {
        let period_duration = period
            .attribute("duration")
            .and_then(parse_iso_duration)
            .unwrap_or(mpd_duration);

        for adapt in period.children().filter(|n| tag(n, "AdaptationSet")) {
            let adapt_type = adapt.attribute("contentType").unwrap_or("");
            let adapt_mime = adapt.attribute("mimeType").unwrap_or("");
            let is_audio = adapt_type == "audio"
                || adapt_mime.starts_with("audio/")
                || adapt_mime.contains("mp4a");
            let adapt_base = child_text(adapt, "BaseURL");
            let adapt_tmpl = child(adapt, "SegmentTemplate");
            let adapt_list = child(adapt, "SegmentList");

            for rep in adapt.children().filter(|n| tag(n, "Representation")) {
                let rep_id = rep.attribute("id").unwrap_or("0");
                let bandwidth = rep
                    .attribute("bandwidth")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0);
                let mime = rep.attribute("mimeType").unwrap_or(adapt_mime);

                let mut url = base.clone();
                if let Some(b) = child_text(rep, "BaseURL").or(adapt_base.clone()) {
                    url = url.join(&b)?;
                }

                let tmpl = child(rep, "SegmentTemplate").or(adapt_tmpl);
                let seg_list = child(rep, "SegmentList").or(adapt_list);
                let (init, segments) =
                    build_dash_segments(&url, rep_id, bandwidth, tmpl, seg_list, period_duration)?;

                streams.push(DashStream {
                    id: rep_id.to_string(),
                    bandwidth,
                    width: rep
                        .attribute("width")
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(0),
                    height: rep
                        .attribute("height")
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(0),
                    codecs: rep
                        .attribute("codecs")
                        .unwrap_or("")
                        .to_string(),
                    audio: is_audio || mime.starts_with("audio/"),
                    init,
                    segments,
                });
            }
        }
    }
    if streams.is_empty() {
        bail!("MPD contains no usable representations");
    }
    Ok(streams)
}

// ---------------------------------------------------------------------------
// selection
// ---------------------------------------------------------------------------

fn pick_variant(variants: &[Variant], quality: Quality) -> Option<&Variant> {
    if variants.is_empty() {
        return None;
    }
    match quality {
        Quality::Best => variants.iter().max_by_key(|v| v.bandwidth),
        Quality::Worst => variants.iter().min_by_key(|v| v.bandwidth),
        Quality::Height(h) => variants
            .iter()
            .min_by_key(|v| (v.height as i64 - h as i64).abs()),
    }
}

fn pick_stream(streams: &[DashStream], quality: Quality, audio: bool) -> Option<&DashStream> {
    let candidates: Vec<&DashStream> = streams.iter().filter(|s| s.audio == audio).collect();
    if candidates.is_empty() {
        return None;
    }
    match quality {
        Quality::Best => candidates.into_iter().max_by_key(|s| s.bandwidth),
        Quality::Worst => candidates.into_iter().min_by_key(|s| s.bandwidth),
        Quality::Height(h) => candidates
            .into_iter()
            .min_by_key(|s| (s.height as i64 - h as i64).abs()),
    }
}

// ---------------------------------------------------------------------------
// downloading
// ---------------------------------------------------------------------------

fn segment_bar(total: usize, label: &str) -> ProgressBar {
    let pb = ProgressBar::new(total as u64);
    pb.set_style(
        ProgressStyle::with_template("{spinner:.green} {msg} [{bar:28.cyan/blue}] {pos}/{len} ({eta})")
            .unwrap()
            .progress_chars("=>-"),
    );
    pb.set_message(label.to_string());
    pb
}

async fn fetch_segment(
    http: &HttpClient,
    seg: &SegmentRef,
    referer: Option<&str>,
) -> Result<Vec<u8>> {
    match seg.range {
        Some((start, len)) => http.fetch_range(&seg.url, start, len, referer).await,
        None => http.fetch_bytes(&seg.url, referer).await,
    }
}

struct DownloadPlan<'a> {
    init: Option<&'a SegmentRef>,
    segments: &'a [SegmentRef],
    referer: Option<&'a str>,
    concurrency: usize,
    progress: Option<std::sync::Arc<crate::engine::Progress>>,
}

async fn download_concat(
    http: Arc<HttpClient>,
    plan: DownloadPlan<'_>,
    out: &Path,
    label: &str,
) -> Result<()> {
    let dir = out.with_extension("segs");
    // do not wipe an existing parts folder: this is what makes a retry resume
    tokio::fs::create_dir_all(&dir).await?;

    let pb = segment_bar(plan.segments.len(), label);
    let sem = Arc::new(Semaphore::new(plan.concurrency.max(1)));
    let referer = plan.referer.map(|s| s.to_string());
    let progress = plan.progress.clone();
    let total_segments = plan.segments.len() as u64;

    let mut existing_bytes = 0u64;
    let mut existing_count = 0u64;
    for i in 0..plan.segments.len() {
        let path = dir.join(format!("{:06}.part", i));
        if let Ok(meta) = tokio::fs::metadata(&path).await {
            existing_bytes += meta.len();
            existing_count += 1;
            pb.inc(1);
        }
    }
    if let Some(progress) = &progress {
        if existing_count > 0 {
            let done = progress.done.fetch_add(
                existing_bytes,
                std::sync::atomic::Ordering::Relaxed,
            ) + existing_bytes;
            progress.total.store(
                done * total_segments / existing_count.max(1),
                std::sync::atomic::Ordering::Relaxed,
            );
        }
    }

    let mut set: JoinSet<Result<usize>> = JoinSet::new();
    for (i, seg) in plan.segments.iter().enumerate() {
        let path = dir.join(format!("{:06}.part", i));
        if path.exists() {
            continue;
        }
        let permit = sem.clone().acquire_owned().await?;
        let http = http.clone();
        let seg = seg.clone();
        let referer = referer.clone();
        let pb2 = pb.clone();
        let progress = progress.clone();
        set.spawn(async move {
            let _permit = permit;
            let mut last_err = None;
            for attempt in 0..3 {
                match fetch_segment(&http, &seg, referer.as_deref()).await {
                    Ok(bytes) => {
                        tokio::fs::write(&path, &bytes).await?;
                        pb2.inc(1);
                        if let Some(progress) = &progress {
                            let done = progress.done.fetch_add(
                                bytes.len() as u64,
                                std::sync::atomic::Ordering::Relaxed,
                            ) + bytes.len() as u64;
                            let completed = pb2.position().max(1);
                            let estimate = done * total_segments / completed;
                            progress
                                .total
                                .store(estimate, std::sync::atomic::Ordering::Relaxed);
                        }
                        return Ok(bytes.len());
                    }
                    Err(e) => {
                        last_err = Some(e);
                        tokio::time::sleep(Duration::from_millis(250 * (attempt + 1))).await;
                    }
                }
            }
            Err(last_err.unwrap_or_else(|| anyhow!("segment failed")))
        });
    }

    while let Some(joined) = set.join_next().await {
        // errors are represented by missing part files and retried below
        let _ = joined;
    }
    pb.finish_and_clear();

    // a single slow/expired segment used to abort the whole movie; retry the
    // missing ones a few more times and keep every successful part
    let total = plan.segments.len();
    for _round in 0..5 {
        let missing: Vec<usize> = (0..total)
            .filter(|i| !dir.join(format!("{:06}.part", i)).exists())
            .collect();
        if missing.is_empty() {
            break;
        }
        for index in missing {
            let seg = &plan.segments[index];
            let path = dir.join(format!("{:06}.part", index));
            let mut saved = false;
            for attempt in 0..5 {
                match fetch_segment(&http, seg, plan.referer).await {
                    Ok(bytes) => {
                        tokio::fs::write(&path, &bytes).await?;
                        pb.inc(1);
                        if let Some(progress) = &plan.progress {
                            let done = progress.done.fetch_add(
                                bytes.len() as u64,
                                std::sync::atomic::Ordering::Relaxed,
                            ) + bytes.len() as u64;
                            let completed = pb.position().max(1);
                            progress.total.store(
                                done * total as u64 / completed,
                                std::sync::atomic::Ordering::Relaxed,
                            );
                        }
                        saved = true;
                        break;
                    }
                    Err(_) => {
                        tokio::time::sleep(Duration::from_millis(400 * (attempt + 1))).await;
                    }
                }
            }
            if !saved {
                // keep going with the other gaps; retry on the next round
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    let missing = (0..total)
        .filter(|i| !dir.join(format!("{:06}.part", i)).exists())
        .count();
    if missing > 0 {
        return Err(anyhow!(
            "{missing} of {total} segments failed; downloaded parts kept in {}",
            dir.display()
        ));
    }

    // init segment first (fMP4), then every media segment in order
    let mut file = tokio::fs::File::create(out).await?;
    if let Some(init) = plan.init {
        let bytes = fetch_segment(&http, init, plan.referer).await?;
        tokio::io::AsyncWriteExt::write_all(&mut file, &bytes).await?;
    }
    for i in 0..plan.segments.len() {
        let mut part = tokio::fs::File::open(dir.join(format!("{:06}.part", i))).await?;
        tokio::io::copy(&mut part, &mut file).await?;
    }
    tokio::io::AsyncWriteExt::flush(&mut file).await?;

    if !plan.segments.is_empty() {
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// ffmpeg
// ---------------------------------------------------------------------------

fn find_in_dir(dir: &Path, name: &str, depth: usize) -> Option<PathBuf> {
    if depth == 0 {
        return None;
    }
    let entries = std::fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file() && path.file_name().map(|n| n.eq_ignore_ascii_case(name)).unwrap_or(false) {
            return Some(path);
        }
        if path.is_dir() {
            if let Some(found) = find_in_dir(&path, name, depth - 1) {
                return Some(found);
            }
        }
    }
    None
}

pub fn find_ffmpeg() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("TDM_FFMPEG") {
        let p = PathBuf::from(explicit);
        if p.is_file() {
            return Some(p);
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        let mut dir = exe.parent().map(|p| p.to_path_buf());
        for _ in 0..5 {
            let Some(current) = dir else { break };
            let tools = current.join("tools").join("ffmpeg");
            if tools.is_dir() {
                if let Some(found) = find_in_dir(&tools, "ffmpeg.exe", 4) {
                    return Some(found);
                }
            }
            dir = current.parent().map(|p| p.to_path_buf());
        }
    }
    if let Ok(path) = std::env::var("PATH") {
        for entry in std::env::split_paths(&path) {
            let candidate = entry.join("ffmpeg.exe");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

async fn run_ffmpeg(args: &[String]) -> Result<()> {
    let ffmpeg = find_ffmpeg().context(
        "ffmpeg not found - put ffmpeg.exe in tools\\ffmpeg or set TDM_FFMPEG",
    )?;
    let mut command = tokio::process::Command::new(ffmpeg);
    command.args(args);
    // CREATE_NO_WINDOW: TDM is a GUI app, so ffmpeg must not flash a console
    #[cfg(windows)]
    command.creation_flags(0x0800_0000);
    let output = command.output().await.context("running ffmpeg")?;
    if !output.status.success() {
        bail!(
            "ffmpeg failed: {}",
            String::from_utf8_lossy(&output.stderr).lines().last().unwrap_or("unknown error")
        );
    }
    Ok(())
}

async fn ensure_parent(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            tokio::fs::create_dir_all(parent).await?;
        }
    }
    Ok(())
}

fn is_mp4_container(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("mp4" | "m4a" | "mov")
    )
}

async fn mux_single(input: &Path, output: &Path) -> Result<()> {
    let mut args = vec![
        "-y".to_string(),
        "-hide_banner".to_string(),
        "-loglevel".to_string(),
        "error".to_string(),
        "-i".to_string(),
        input.to_string_lossy().into(),
        "-c".to_string(),
        "copy".to_string(),
    ];
    if is_mp4_container(output) {
        args.push("-movflags".to_string());
        args.push("+faststart".to_string());
    }
    args.push(output.to_string_lossy().into());
    run_ffmpeg(&args).await
}

async fn mux_audio_video(video: &Path, audio: &Path, output: &Path) -> Result<()> {
    let mut args = vec![
        "-y".to_string(),
        "-hide_banner".to_string(),
        "-loglevel".to_string(),
        "error".to_string(),
        "-i".to_string(),
        video.to_string_lossy().into(),
        "-i".to_string(),
        audio.to_string_lossy().into(),
        "-map".to_string(),
        "0:v:0".to_string(),
        "-map".to_string(),
        "1:a:0".to_string(),
        "-c".to_string(),
        "copy".to_string(),
        "-shortest".to_string(),
    ];
    if is_mp4_container(output) {
        args.push("-movflags".to_string());
        args.push("+faststart".to_string());
    }
    args.push(output.to_string_lossy().into());
    run_ffmpeg(&args).await
}

// ---------------------------------------------------------------------------
// entry points
// ---------------------------------------------------------------------------

fn default_output(url: &Url, cfg: &Config, audio_only: bool) -> PathBuf {
    let last = url
        .path_segments()
        .and_then(|s| s.filter(|p| !p.is_empty()).last())
        .unwrap_or("video");
    let stem = last.split('.').next().unwrap_or("video");
    let stem = if stem.is_empty() || stem.eq_ignore_ascii_case("index") || stem.eq_ignore_ascii_case("manifest") {
        "video"
    } else {
        stem
    };
    let name = sanitize_filename(&format!("{}.{}", stem, if audio_only { "m4a" } else { "mp4" }));
    Path::new(&cfg.download_dir).join(name)
}

/// Find `.m3u8` / `.mpd` URLs inside an HTML/JS page (simple sites expose them
/// directly in the markup; players that build URLs at runtime need the browser
/// extension instead).
pub fn find_manifests(text: &str, base: &Url) -> Vec<String> {
    let text = text.replace("\\/", "/");
    let bytes = text.as_bytes();
    let mut found = Vec::new();
    for needle in [".m3u8", ".mpd"] {
        let mut start = 0usize;
        while let Some(rel) = text[start..].find(needle) {
            let end = start + rel + needle.len();
            let mut begin = end;
            while begin > 0 {
                let c = bytes[begin - 1];
                if matches!(
                    c,
                    b'"' | b'\'' | b'(' | b')' | b' ' | b'\n' | b'\r' | b'\t' | b'=' | b',' | b'<' | b'>'
                ) {
                    break;
                }
                begin -= 1;
            }
            let raw = &text[begin..end];
            if raw.len() > needle.len() {
                if let Ok(abs) = base.join(raw) {
                    let s = abs.to_string();
                    if !found.contains(&s) {
                        found.push(s);
                    }
                }
            }
            start = end;
        }
    }
    found
}

pub async fn download(opts: &VideoOptions, cfg: &Config) -> Result<PathBuf> {
    let http = Arc::new(HttpClient::new(cfg)?);
    let url = Url::parse(&opts.url).with_context(|| format!("invalid URL: {}", opts.url))?;
    let text = http
        .fetch_text(&opts.url, opts.referer.as_deref())
        .await
        .context("fetching URL")?;

    if text.trim_start().starts_with("#EXTM3U") {
        return download_hls(http, &url, &text, opts, cfg).await;
    }
    if text.contains("<MPD") {
        return download_dash(http, &url, &text, opts, cfg).await;
    }

    // Not a manifest: treat it as a page and look for embedded manifest URLs.
    let candidates = find_manifests(&text, &url);
    if candidates.is_empty() {
        bail!(
            "URL is neither an HLS/DASH manifest nor a page containing one \
             (use the manifest URL from the browser's network tab)"
        );
    }
    if opts.list_only || candidates.len() > 1 {
        println!("found {} media manifest(s):", candidates.len());
        for c in &candidates {
            println!("  {}", c);
        }
    }
    let chosen = candidates[0].clone();
    println!("using: {}", chosen);
    let manifest_url = Url::parse(&chosen)?;
    let body = http
        .fetch_text(&chosen, opts.referer.as_deref())
        .await
        .context("fetching discovered manifest")?;
    if body.trim_start().starts_with("#EXTM3U") {
        download_hls(http, &manifest_url, &body, opts, cfg).await
    } else if body.contains("<MPD") {
        download_dash(http, &manifest_url, &body, opts, cfg).await
    } else {
        bail!("discovered URL {} is not a manifest", chosen)
    }
}

async fn download_hls(
    http: Arc<HttpClient>,
    url: &Url,
    text: &str,
    opts: &VideoOptions,
    cfg: &Config,
) -> Result<PathBuf> {
    let (variants, audio_groups) = parse_master(text, url);
    let mut audio_group = None;

    let media_text;
    let base;
    if is_master(text) {
        let Some(variant) = pick_variant(&variants, opts.quality) else {
            bail!("master playlist has no variants");
        };
        if opts.list_only {
            for v in &variants {
                println!(
                    "  {:>5}x{:<5} {:>10}  {}",
                    v.width, v.height, human_bytes(v.bandwidth / 8), v.url
                );
            }
            return Ok(PathBuf::new());
        }
        audio_group = variant.audio_group.clone();
        base = Url::parse(&variant.url)?;
        media_text = http
            .fetch_text(&variant.url, opts.referer.as_deref())
            .await
            .context("fetching media playlist")?;
    } else {
        base = url.clone();
        media_text = text.to_string();
    }

    let media = parse_media_playlist(&media_text, &base)?;
    if media.encrypted {
        bail!("this HLS stream is AES-128 encrypted (not supported)");
    }
    if !media.ended {
        bail!("live HLS stream (no #EXT-X-ENDLIST) is not supported");
    }
    if media.segments.is_empty() {
        bail!("playlist contains no segments");
    }

    let temp = temp_dir_for(opts);
    tokio::fs::create_dir_all(&temp).await?;
    let video_path = temp.join("video.bin");

    let label = format!("{} segments", media.segments.len());
    download_concat(
        http.clone(),
        DownloadPlan {
            init: media.init.as_ref(),
            segments: &media.segments,
            referer: opts.referer.as_deref(),
            concurrency: opts.concurrency,
            progress: opts.progress.clone(),
        },
        &video_path,
        &label,
    )
    .await?;

    // separate audio rendition, when the master references one
    let audio_path = if !opts.audio_only {
        if let Some(group) = audio_group {
            if let Some(audio_url) = audio_groups.get(&group) {
                let audio_base = Url::parse(audio_url)?;
                let audio_text = http
                    .fetch_text(audio_url, opts.referer.as_deref())
                    .await?;
                let audio_media = parse_media_playlist(&audio_text, &audio_base)?;
                if audio_media.encrypted {
                    bail!("audio rendition is encrypted (not supported)");
                }
                let path = temp.join("audio.bin");
                download_concat(
                    http.clone(),
                    DownloadPlan {
                        init: audio_media.init.as_ref(),
                        segments: &audio_media.segments,
                        referer: opts.referer.as_deref(),
                        concurrency: opts.concurrency,
                        progress: opts.progress.clone(),
                    },
                    &path,
                    "audio segments",
                )
                .await?;
                Some(path)
            } else {
                None
            }
        } else {
            None
        }
    } else {
        None
    };

    let out = opts
        .out
        .clone()
        .unwrap_or_else(|| default_output(url, cfg, opts.audio_only));

    let result = if opts.audio_only {
        let audio_out = out.with_extension("m4a");
        ensure_parent(&audio_out).await?;
        mux_single(&video_path, &audio_out).await?;
        Ok(audio_out)
    } else if let Some(audio) = audio_path {
        let target = out.with_extension("mp4");
        ensure_parent(&target).await?;
        mux_audio_video(&video_path, &audio, &target).await?;
        Ok(target)
    } else {
        let target = out.with_extension("mp4");
        ensure_parent(&target).await?;
        mux_single(&video_path, &target).await?;
        Ok(target)
    };

    if !opts.keep_temp {
        let _ = tokio::fs::remove_dir_all(&temp).await;
    }
    result
}

async fn download_dash(
    http: Arc<HttpClient>,
    url: &Url,
    text: &str,
    opts: &VideoOptions,
    cfg: &Config,
) -> Result<PathBuf> {
    let streams = parse_mpd(text, url)?;

    if opts.list_only {
        for s in &streams {
            println!(
                "  [{}] {:>5}x{:<5} {:>10}  {}  ({} segments)  {}",
                if s.audio { "audio" } else { "video" },
                s.width,
                s.height,
                human_bytes(s.bandwidth / 8),
                s.codecs,
                s.segments.len(),
                s.id
            );
        }
        return Ok(PathBuf::new());
    }

    let video = if opts.audio_only {
        None
    } else {
        pick_stream(&streams, opts.quality, false)
    };
    let audio = pick_stream(&streams, Quality::Best, true);

    if video.is_none() && audio.is_none() {
        bail!("MPD has no matching video/audio representation");
    }

    let temp = temp_dir_for(opts);
    tokio::fs::create_dir_all(&temp).await?;

    let video_path = if let Some(v) = video {
        let path = temp.join("video.bin");
        download_concat(
            http.clone(),
            DownloadPlan {
                init: v.init.as_ref(),
                segments: &v.segments,
                referer: opts.referer.as_deref(),
                concurrency: opts.concurrency,
                progress: opts.progress.clone(),
            },
            &path,
            "video segments",
        )
        .await?;
        Some(path)
    } else {
        None
    };

    let audio_path = if let Some(a) = audio {
        let path = temp.join("audio.bin");
        download_concat(
            http.clone(),
            DownloadPlan {
                init: a.init.as_ref(),
                segments: &a.segments,
                referer: opts.referer.as_deref(),
                concurrency: opts.concurrency,
                progress: opts.progress.clone(),
            },
            &path,
            "audio segments",
        )
        .await?;
        Some(path)
    } else {
        None
    };

    let out = opts
        .out
        .clone()
        .unwrap_or_else(|| default_output(url, cfg, opts.audio_only));

    let result = match (video_path, audio_path) {
        (Some(v), Some(a)) => {
            let target = out.with_extension("mp4");
            mux_audio_video(&v, &a, &target).await?;
            Ok(target)
        }
        (Some(v), None) => {
            let target = out.with_extension("mp4");
            mux_single(&v, &target).await?;
            Ok(target)
        }
        (None, Some(a)) => {
            let target = out.with_extension("m4a");
            mux_single(&a, &target).await?;
            Ok(target)
        }
        (None, None) => bail!("nothing to download"),
    };

    if !opts.keep_temp {
        let _ = tokio::fs::remove_dir_all(&temp).await;
    }
    result
}

/// Download a captured video URL and audio URL, then mux them.
/// Used when the browser plays separate video and audio streams.
pub async fn download_pair(
    video_url: &str,
    audio_url: &str,
    out: &Path,
    referer: Option<&str>,
    cookie: Option<&str>,
    user_agent: Option<&str>,
    cfg: &Config,
) -> Result<PathBuf> {
    let http = HttpClient::new(cfg)?;
    let temp = std::env::temp_dir().join(format!("tdm-pair-{}", uuid::Uuid::new_v4().simple()));
    tokio::fs::create_dir_all(&temp).await?;
    let video = temp.join("video.bin");
    let audio = temp.join("audio.bin");

    let (v, a) = tokio::join!(
        http.download_file(video_url, &video, referer, cookie, user_agent),
        http.download_file(audio_url, &audio, referer, cookie, user_agent)
    );
    v.context("downloading video stream")?;
    a.context("downloading audio stream")?;

    let target = out.with_extension("mp4");
    ensure_parent(&target).await?;
    let result = mux_audio_video(&video, &audio, &target).await;
    let _ = tokio::fs::remove_dir_all(&temp).await;
    result?;
    Ok(target)
}

/// Mux raw recorded tracks (browser MediaSource chunks) into a playable file.
pub async fn mux_recorded(video: Option<&Path>, audio: Option<&Path>, out: &Path) -> Result<()> {
    ensure_parent(out).await?;
    match (video, audio) {
        (Some(v), Some(a)) => mux_audio_video(v, a, out).await,
        (Some(v), None) => mux_single(v, out).await,
        (None, Some(a)) => mux_single(a, out).await,
        (None, None) => bail!("nothing to mux"),
    }
}

fn temp_dir_for(opts: &VideoOptions) -> PathBuf {
    if let Some(work) = &opts.work_dir {
        return work.clone();
    }
    let stem = opts
        .out
        .as_ref()
        .and_then(|p| p.file_stem())
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "tdm-video".to_string());
    std::env::temp_dir().join(format!("{}-{}", stem, uuid::Uuid::new_v4().simple()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_master_playlist() {
        let text = "#EXTM3U\n\
                    #EXT-X-STREAM-INF:BANDWIDTH=800000,RESOLUTION=640x360,CODECS=\"avc1.42e01e,mp4a.40.2\"\n\
                    low/index.m3u8\n\
                    #EXT-X-STREAM-INF:BANDWIDTH=2400000,RESOLUTION=1280x720\n\
                    high/index.m3u8\n";
        let base = Url::parse("https://example.com/video/master.m3u8").unwrap();
        let (variants, _) = parse_master(text, &base);
        assert_eq!(variants.len(), 2);
        assert_eq!(variants[1].height, 720);
        assert_eq!(variants[1].url, "https://example.com/video/high/index.m3u8");
        assert_eq!(pick_variant(&variants, Quality::Best).unwrap().height, 720);
        assert_eq!(pick_variant(&variants, Quality::Worst).unwrap().height, 360);
    }

    #[test]
    fn parses_media_playlist_with_map_and_ranges() {
        let text = "#EXTM3U\n\
                    #EXT-X-MAP:URI=\"init.mp4\",BYTERANGE=\"720@0\"\n\
                    #EXTINF:2.0,\n\
                    #EXT-X-BYTERANGE:1000@720\n\
                    main.mp4\n\
                    #EXTINF:2.0,\n\
                    #EXT-X-BYTERANGE:900\n\
                    main.mp4\n\
                    #EXT-X-ENDLIST\n";
        let base = Url::parse("https://example.com/a/playlist.m3u8").unwrap();
        let media = parse_media_playlist(text, &base).unwrap();
        assert!(media.ended);
        assert_eq!(media.init.as_ref().unwrap().range, Some((0, 720)));
        assert_eq!(media.segments.len(), 2);
        assert_eq!(media.segments[0].range, Some((720, 1000)));
        // second range without offset continues after the first
        assert_eq!(media.segments[1].range, Some((1720, 900)));
    }

    #[test]
    fn parses_dash_template() {
        let mpd = r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" mediaPresentationDuration="PT6S" type="static">
  <Period>
    <AdaptationSet contentType="video" mimeType="video/mp4">
      <SegmentTemplate media="seg-$Number$.m4s" initialization="init.mp4" startNumber="1" duration="2" timescale="1"/>
      <Representation id="v1" bandwidth="1500000" width="1280" height="720" codecs="avc1.64001f"/>
    </AdaptationSet>
    <AdaptationSet contentType="audio" mimeType="audio/mp4">
      <SegmentTemplate media="a-$Number$.m4s" initialization="ainit.mp4" startNumber="1" duration="2" timescale="1"/>
      <Representation id="a1" bandwidth="128000" codecs="mp4a.40.2"/>
    </AdaptationSet>
  </Period>
</MPD>"#;
        let base = Url::parse("https://example.com/dash/manifest.mpd").unwrap();
        let streams = parse_mpd(mpd, &base).unwrap();
        assert_eq!(streams.len(), 2);
        let video = streams.iter().find(|s| !s.audio).unwrap();
        assert_eq!(video.segments.len(), 3);
        assert_eq!(video.segments[0].url, "https://example.com/dash/seg-1.m4s");
        assert_eq!(video.init.as_ref().unwrap().url, "https://example.com/dash/init.mp4");
        let audio = streams.iter().find(|s| s.audio).unwrap();
        assert_eq!(audio.segments.len(), 3);
    }

    #[test]
    fn iso_duration() {
        assert_eq!(parse_iso_duration("PT6S"), Some(6.0));
        assert_eq!(parse_iso_duration("PT1H2M3.5S"), Some(3723.5));
        assert_eq!(parse_iso_duration("bogus"), None);
    }

    #[test]
    fn finds_manifests_in_html() {
        let html = r#"<script>var src="https://cdn.example.com/a/b/master.m3u8";</script>
                      <source src='\/dash\/manifest.mpd'>"#;
        let base = Url::parse("https://example.com/watch").unwrap();
        let found = find_manifests(html, &base);
        assert!(found.contains(&"https://cdn.example.com/a/b/master.m3u8".to_string()));
        assert!(found.contains(&"https://example.com/dash/manifest.mpd".to_string()));
    }

    #[test]
    fn template_substitution() {
        assert_eq!(
            substitute("$RepresentationID$/seg-$Number%04d$.m4s", "v1", 1000, 7, 14),
            "v1/seg-0007.m4s"
        );
        assert_eq!(substitute("t$Time$.m4s", "a", 1, 1, 42), "t42.m4s");
    }
}
