//! Segmented download engine.
//!
//! One `Download` is split into N byte ranges ("connections"). Each worker
//! asks for its own range, writes at its own offset and advances an atomic counter.
//! Progress is checkpointed to the store every tick so a paused or crashed download
//! resumes from the last completed bytes instead of restarting.

use crate::config::Config;
use crate::http::HttpClient;
use crate::model::{Download, Segment, Status};
use crate::speed::SpeedLimiter;
use crate::store::Store;
use crate::util::{category_for, now_unix, sanitize_filename, sha256_file, unique_path};
use anyhow::{anyhow, bail, Context, Result};
use futures_util::StreamExt;
use rand::Rng;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncSeekExt, AsyncWriteExt};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

const TICK: Duration = Duration::from_millis(400);
const READ_TIMEOUT: Duration = Duration::from_secs(120);
const MIN_SEGMENT: u64 = 256 * 1024;

#[derive(Default)]
pub struct Progress {
    pub total: AtomicU64,
    pub done: AtomicU64,
    pub speed: AtomicU64,
}

impl Progress {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn total(&self) -> u64 {
        self.total.load(Ordering::Relaxed)
    }

    pub fn done(&self) -> u64 {
        self.done.load(Ordering::Relaxed)
    }

    pub fn speed(&self) -> u64 {
        self.speed.load(Ordering::Relaxed)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Completed,
    Paused,
    Failed,
}

struct SegState {
    start: u64,
    end: u64,
    done: AtomicU64,
}

struct WorkerCtx {
    http: Arc<HttpClient>,
    dl: Download,
    part: PathBuf,
    segments: Arc<Vec<SegState>>,
    index: usize,
    ctl: CancellationToken,
    progress: Arc<Progress>,
    limiter: Arc<SpeedLimiter>,
    retries: u32,
    single_stream: bool,
}

pub struct Engine {
    pub cfg: Arc<Config>,
    pub http: Arc<HttpClient>,
    pub limiter: Arc<SpeedLimiter>,
    pub store: Store,
}

impl Engine {
    pub fn new(cfg: Arc<Config>, store: Store) -> Result<Self> {
        let http = Arc::new(HttpClient::new(&cfg)?);
        let limiter = Arc::new(SpeedLimiter::new(cfg.speed_limit));
        Ok(Self { cfg, http, limiter, store })
    }

    /// Download (or resume) one item. Always leaves the record in a terminal or paused state.
    pub async fn run(
        &self,
        dl: &mut Download,
        ctl: CancellationToken,
        progress: Arc<Progress>,
    ) -> Outcome {
        dl.status = Status::Downloading;
        dl.error = None;
        let _ = self.store.save(dl);

        let result = self.execute(dl, &ctl, &progress).await;

        let outcome = if ctl.is_cancelled() {
            Outcome::Paused
        } else {
            match result {
                Ok(()) => Outcome::Completed,
                Err(e) => {
                    dl.error = Some(format!("{:#}", e));
                    Outcome::Failed
                }
            }
        };

        match outcome {
            Outcome::Completed => {
                dl.status = Status::Completed;
                if dl.size == 0 {
                    dl.size = dl.downloaded;
                }
                dl.downloaded = dl.size;
                dl.error = None;
            }
            Outcome::Paused => {
                dl.status = Status::Paused;
            }
            Outcome::Failed => {
                dl.status = Status::Error;
            }
        }
        dl.updated_at = now_unix();
        let _ = self.store.save(dl);
        progress.speed.store(0, Ordering::Relaxed);
        outcome
    }

    async fn execute(
        &self,
        dl: &mut Download,
        ctl: &CancellationToken,
        progress: &Arc<Progress>,
    ) -> Result<()> {
        // ---- 1. discover size / range support if this is a fresh download ----
        if dl.segments.is_empty() {
            self.probe_into(dl).await?;
        }

        if dl.size == 0 {
            // unknown length: stream once until EOF
            dl.ranges_supported = true;
            dl.segments = vec![Segment { start: 0, end: 0, done: 0 }];
        } else if dl.segments.is_empty() {
            dl.segments = split_segments(dl.size, dl.connections);
        }
        if !dl.ranges_supported && dl.size > 0 {
            dl.connections = 1;
            dl.segments = vec![Segment { start: 0, end: dl.size, done: 0 }];
        }

        tokio::fs::create_dir_all(&dl.dir)
            .await
            .with_context(|| format!("creating download directory {}", dl.dir))?;

        let single_stream = dl.size == 0 || !dl.ranges_supported;
        let part = dl.part_path();

        // ---- 2. allocate the target file ----
        {
            let file = tokio::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .open(&part)
                .await
                .with_context(|| format!("opening {}", part.display()))?;
            if single_stream {
                file.set_len(0).await?;
            } else if dl.size > 0 {
                let existing = file.metadata().await?.len();
                if existing != dl.size {
                    file.set_len(dl.size).await?;
                }
            }
        }

        progress.total.store(dl.size, Ordering::Relaxed);
        progress.done.store(0, Ordering::Relaxed);
        // segments were reset on a fresh start; a resume keeps its own counters
        let initial_done: u64 = dl.segments.iter().map(|s| s.done).sum();
        dl.downloaded = initial_done;
        progress.done.store(initial_done, Ordering::Relaxed);
        self.store.save(dl)?;

        // ---- 3. workers ----
        let states: Arc<Vec<SegState>> = Arc::new(
            dl.segments
                .iter()
                .map(|s| SegState { start: s.start, end: s.end, done: AtomicU64::new(s.done) })
                .collect(),
        );

        // child token: an error cancels the other workers without marking the download paused
        let work_ctl = ctl.child_token();
        let mut set: JoinSet<Result<()>> = JoinSet::new();
        let worker_count = if single_stream { 1 } else { states.len() };
        for index in 0..worker_count {
            let ctx = WorkerCtx {
                http: self.http.clone(),
                dl: dl.clone(),
                part: part.clone(),
                segments: states.clone(),
                index,
                ctl: work_ctl.clone(),
                progress: progress.clone(),
                limiter: self.limiter.clone(),
                retries: self.cfg.retries,
                single_stream,
            };
            set.spawn(download_worker(ctx));
        }

        // ---- 4. monitor: checkpoint progress, compute speed ----
        let mut first_error: Option<String> = None;
        let mut last_tick = Instant::now();
        let mut last_bytes = progress.done();
        loop {
            tokio::select! {
                _ = tokio::time::sleep(TICK) => {}
                joined = set.join_next(), if !set.is_empty() => {
                    match joined {
                        Some(Ok(Ok(()))) => {}
                        Some(Ok(Err(e))) => {
                            if first_error.is_none() {
                                first_error = Some(format!("{:#}", e));
                                work_ctl.cancel();
                            }
                        }
                        Some(Err(join_err)) => {
                            if first_error.is_none() {
                                first_error = Some(format!("worker crashed: {}", join_err));
                                work_ctl.cancel();
                            }
                        }
                        None => {}
                    }
                }
            }

            // push atomic counters back into the persisted record
            let mut sum = 0u64;
            for (i, st) in states.iter().enumerate() {
                let done = st.done.load(Ordering::Relaxed);
                if let Some(seg) = dl.segments.get_mut(i) {
                    seg.done = done;
                }
                sum += done;
            }
            dl.downloaded = sum;
            dl.updated_at = now_unix();
            let now = Instant::now();
            let dt = now.duration_since(last_tick).as_secs_f64();
            if dt >= 0.3 {
                let speed = ((sum.saturating_sub(last_bytes)) as f64 / dt) as u64;
                progress.speed.store(speed, Ordering::Relaxed);
                last_tick = now;
                last_bytes = sum;
            }
            let _ = self.store.save(dl);

            if ctl.is_cancelled() {
                // let workers leave their loops before we return
                let _ = tokio::time::timeout(Duration::from_secs(20), async {
                    while set.join_next().await.is_some() {}
                })
                .await;
                return Ok(());
            }
            if set.is_empty() {
                break;
            }
        }

        // final sync
        let mut sum = 0u64;
        for (i, st) in states.iter().enumerate() {
            let done = st.done.load(Ordering::Relaxed);
            if let Some(seg) = dl.segments.get_mut(i) {
                seg.done = done;
            }
            sum += done;
        }
        dl.downloaded = sum;

        if let Some(err) = first_error {
            bail!("{}", err);
        }
        if ctl.is_cancelled() {
            return Ok(());
        }

        // ---- 5. finalize ----
        if dl.size > 0 && dl.downloaded < dl.size {
            bail!(
                "incomplete download: {} of {} bytes",
                dl.downloaded,
                dl.size
            );
        }
        if dl.size == 0 {
            dl.size = dl.downloaded;
        }

        let target = unique_path(Path::new(&dl.dir), &dl.file_name);
        if let Some(expected) = dl.checksum.clone() {
            let actual = tokio::task::spawn_blocking({
                let part = part.clone();
                move || sha256_file(&part)
            })
            .await??;
            if !actual.eq_ignore_ascii_case(expected.trim()) {
                bail!("checksum mismatch: expected {}, got {}", expected, actual);
            }
        }

        // Windows rename fails if the destination exists; unique_path guarantees it does not
        tokio::fs::rename(&part, &target)
            .await
            .with_context(|| format!("moving {} -> {}", part.display(), target.display()))?;
        if let Some(name) = target.file_name().and_then(|n| n.to_str()) {
            dl.file_name = name.to_string();
        }
        dl.status = Status::Completed;
        dl.updated_at = now_unix();
        self.store.save(dl)?;
        Ok(())
    }

    async fn probe_into(&self, dl: &mut Download) -> Result<()> {
        let info = self.http.probe(dl).await?;
        dl.final_url = Some(info.final_url);
        dl.size = info.size;
        dl.ranges_supported = info.ranges;
        if !dl.name_locked {
            if let Some(name) = info.filename {
                let clean = sanitize_filename(&name);
                if !clean.is_empty() {
                    dl.file_name = clean;
                }
            }
        }
        dl.etag = info.etag;
        dl.last_modified = info.last_modified;
        dl.content_type = info.content_type;
        if dl.category.is_empty() {
            dl.category = category_for(&dl.file_name).to_string();
        }
        if dl.size > 0 && dl.ranges_supported {
            dl.segments = split_segments(dl.size, dl.connections);
        }
        Ok(())
    }
}

/// Split `size` into at most `connections` ranges. `end` is exclusive.
pub fn split_segments(size: u64, connections: u32) -> Vec<Segment> {
    if size == 0 {
        return Vec::new();
    }
    let max_by_size = (size / MIN_SEGMENT).max(1);
    let count = (connections.max(1) as u64).min(max_by_size).max(1);
    let chunk = size / count;
    let mut segments = Vec::with_capacity(count as usize);
    let mut pos = 0u64;
    for i in 0..count {
        let end = if i == count - 1 { size } else { pos + chunk };
        segments.push(Segment { start: pos, end, done: 0 });
        pos = end;
    }
    segments
}

async fn read_chunk<S>(stream: &mut S) -> Result<Option<bytes::Bytes>>
where
    S: futures_util::Stream<Item = reqwest::Result<bytes::Bytes>> + Unpin,
{
    match tokio::time::timeout(READ_TIMEOUT, stream.next()).await {
        Err(_) => bail!("connection stalled for {}s", READ_TIMEOUT.as_secs()),
        Ok(None) => Ok(None),
        Ok(Some(Ok(chunk))) => Ok(Some(chunk)),
        Ok(Some(Err(e))) => Err(e).context("reading response body"),
    }
}

async fn download_worker(ctx: WorkerCtx) -> Result<()> {
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .open(&ctx.part)
        .await?;

    if ctx.single_stream {
        return single_stream(ctx, file).await;
    }

    let seg = &ctx.segments[ctx.index];
    let total_len = seg.end.saturating_sub(seg.start);
    let mut attempt: u32 = 0;
    let mut last_error: Option<anyhow::Error> = None;

    loop {
        if ctx.ctl.is_cancelled() {
            return Ok(());
        }
        let done = seg.done.load(Ordering::Relaxed);
        if done >= total_len {
            return Ok(());
        }
        let start = seg.start + done;
        let end = seg.end - 1;
        let before = done;

        let attempt_result = async {
            let resp = ctx.http.get_range(&ctx.dl, start, end).await?;
            let mut stream = resp.bytes_stream();
            while let Some(chunk) = read_chunk(&mut stream).await? {
                if ctx.ctl.is_cancelled() {
                    return Ok::<(), anyhow::Error>(());
                }
                if chunk.is_empty() {
                    continue;
                }
                ctx.limiter.acquire(chunk.len()).await;
                let pos = seg.start + seg.done.load(Ordering::Relaxed);
                file.seek(std::io::SeekFrom::Start(pos)).await?;
                file.write_all(&chunk).await?;
                seg.done.fetch_add(chunk.len() as u64, Ordering::Relaxed);
                ctx.progress
                    .done
                    .fetch_add(chunk.len() as u64, Ordering::Relaxed);
            }
            Ok(())
        }
        .await;

        match attempt_result {
            Ok(()) => {
                if ctx.ctl.is_cancelled() {
                    return Ok(());
                }
                let now_done = seg.done.load(Ordering::Relaxed);
                if now_done >= total_len {
                    return Ok(());
                }
                if now_done > before {
                    attempt = 0;
                } else {
                    attempt += 1;
                }
            }
            Err(e) => {
                if ctx.ctl.is_cancelled() {
                    return Ok(());
                }
                last_error = Some(e);
                attempt += 1;
            }
        }

        if attempt > ctx.retries {
            return Err(anyhow!(
                "connection {} failed after {} attempts: {}",
                ctx.index + 1,
                attempt,
                last_error
                    .map(|e| format!("{:#}", e))
                    .unwrap_or_else(|| "no progress".into())
            ));
        }
        let backoff = 250u64
            .saturating_mul(2u64.pow(attempt.min(5)))
            .min(8_000);
        let jitter = rand::thread_rng().gen_range(0..250);
        tokio::time::sleep(Duration::from_millis(backoff + jitter)).await;
    }
}

async fn single_stream(ctx: WorkerCtx, mut file: tokio::fs::File) -> Result<()> {
    let resp = ctx.http.get_full(&ctx.dl).await?;
    let mut stream = resp.bytes_stream();
    let mut pos = 0u64;
    while let Some(chunk) = read_chunk(&mut stream).await? {
        if ctx.ctl.is_cancelled() {
            return Ok(());
        }
        if chunk.is_empty() {
            continue;
        }
        ctx.limiter.acquire(chunk.len()).await;
        file.seek(std::io::SeekFrom::Start(pos)).await?;
        file.write_all(&chunk).await?;
        pos += chunk.len() as u64;
        if let Some(seg) = ctx.segments.first() {
            seg.done.store(pos, Ordering::Relaxed);
        }
        ctx.progress.done.store(pos, Ordering::Relaxed);
        ctx.progress.total.store(pos, Ordering::Relaxed);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_covers_whole_file() {
        let segs = split_segments(1_000_000, 8);
        assert_eq!(segs.first().unwrap().start, 0);
        assert_eq!(segs.last().unwrap().end, 1_000_000);
        for pair in segs.windows(2) {
            assert_eq!(pair[0].end, pair[1].start);
        }
    }

    #[test]
    fn split_handles_tiny_files() {
        let segs = split_segments(100, 8);
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].end, 100);
    }

    #[test]
    fn split_zero_size() {
        assert!(split_segments(0, 8).is_empty());
    }

    #[test]
    fn split_never_exceeds_requested_connections() {
        let segs = split_segments(10 * 1024 * 1024, 4);
        assert_eq!(segs.len(), 4);
    }
}
