//! End-to-end tests against a local HTTP server with real Range support.

use axum::body::Body;
use axum::extract::State;
use axum::http::{header, HeaderMap, Response, StatusCode};
use axum::routing::get;
use axum::Router;
use bytes::Bytes;
use tdm::config::Config;
use tdm::engine::{Engine, Outcome, Progress};
use tdm::model::{Download, Status};
use tdm::store::Store;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

const CHUNK: usize = 16 * 1024;

struct TestState {
    data: Arc<Vec<u8>>,
    ranges: bool,
    chunk_delay_ms: u64,
    fail_first: usize,
    failures: AtomicUsize,
    range_requests: Arc<AtomicUsize>,
}

struct TestServer {
    port: u16,
    range_requests: Arc<AtomicUsize>,
}

fn payload(size: usize) -> Vec<u8> {
    (0..size).map(|i| ((i * 31 + 7) % 251) as u8).collect()
}

impl TestServer {
    async fn start(size: usize, ranges: bool, chunk_delay_ms: u64, fail_first: usize) -> Self {
        let range_requests = Arc::new(AtomicUsize::new(0));
        let state = Arc::new(TestState {
            data: Arc::new(payload(size)),
            ranges,
            chunk_delay_ms,
            fail_first,
            failures: AtomicUsize::new(0),
            range_requests: range_requests.clone(),
        });
        let app = Router::new()
            .route("/file.bin", get(handler))
            .with_state(state);
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self { port, range_requests }
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}/file.bin", self.port)
    }
}

async fn handler(State(state): State<Arc<TestState>>, headers: HeaderMap) -> Response<Body> {
    let raw_range = headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let is_probe = raw_range.as_deref() == Some("bytes=0-0");

    // fail a few real range requests to exercise retry/backoff (never the probe)
    if !is_probe && state.fail_first > 0 {
        let n = state.failures.fetch_add(1, Ordering::SeqCst);
        if n < state.fail_first {
            return Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(Body::empty())
                .unwrap();
        }
    }

    let total = state.data.len() as u64;
    let range = raw_range
        .as_deref()
        .and_then(|v| v.strip_prefix("bytes="))
        .and_then(|v| v.split_once('-'))
        .and_then(|(a, b)| {
            let start: u64 = a.parse().ok()?;
            let end: u64 = if b.is_empty() { total.saturating_sub(1) } else { b.parse().ok()? };
            Some((start.min(total), (end + 1).min(total)))
        });

    let (start, end, status) = match (range, state.ranges) {
        (Some((s, e)), true) => {
            state.range_requests.fetch_add(1, Ordering::SeqCst);
            (s, e, StatusCode::PARTIAL_CONTENT)
        }
        _ => (0, total, StatusCode::OK),
    };

    let slice = state.data[start as usize..end as usize].to_vec();
    let chunk_delay = state.chunk_delay_ms;
    let stream = futures_util::stream::unfold(0usize, move |pos| {
        let slice = slice.clone();
        async move {
            if pos >= slice.len() {
                return None;
            }
            let next = (pos + CHUNK).min(slice.len());
            if chunk_delay > 0 {
                tokio::time::sleep(Duration::from_millis(chunk_delay)).await;
            }
            Some((Ok::<_, std::io::Error>(Bytes::copy_from_slice(&slice[pos..next])), next))
        }
    });

    let mut builder = Response::builder().status(status);
    if status == StatusCode::PARTIAL_CONTENT {
        builder = builder.header(
            header::CONTENT_RANGE,
            format!("bytes {}-{}/{}", start, end.saturating_sub(1), total),
        );
    }
    builder
        .header(header::CONTENT_LENGTH, (end - start).to_string())
        .header(header::ACCEPT_RANGES, "bytes")
        .body(Body::from_stream(stream))
        .unwrap()
}

fn test_config(dir: &std::path::Path, connections: u32) -> Arc<Config> {
    let mut cfg = Config::default();
    cfg.download_dir = dir.to_string_lossy().to_string();
    cfg.connections = connections;
    cfg.retries = 5;
    cfg.speed_limit = 0;
    Arc::new(cfg)
}

async fn run_download(
    engine: Arc<Engine>,
    mut dl: Download,
    ctl: CancellationToken,
) -> (Outcome, Download) {
    let progress = Progress::new();
    let outcome = engine.run(&mut dl, ctl, progress).await;
    (outcome, dl)
}

#[tokio::test]
async fn downloads_in_parallel_ranges() {
    let server = TestServer::start(2 * 1024 * 1024, true, 0, 0).await;
    let tmp = tempfile::tempdir().unwrap();
    let cfg = test_config(tmp.path(), 4);
    let store = Store::at(tmp.path().join("state"));
    let engine = Arc::new(Engine::new(cfg, store).unwrap());

    let dl = Download::new(server.url(), tmp.path().to_string_lossy().into(), "file.bin".into(), 4);
    let (outcome, dl) = run_download(engine, dl, CancellationToken::new()).await;

    assert_eq!(outcome, Outcome::Completed, "error: {:?}", dl.error);
    assert_eq!(dl.status, Status::Completed);
    assert_eq!(dl.size, 2 * 1024 * 1024);
    let saved = dl.final_path();
    assert!(saved.exists());
    assert_eq!(std::fs::read(&saved).unwrap(), payload(2 * 1024 * 1024));
    assert!(
        server.range_requests.load(Ordering::SeqCst) >= 2,
        "expected multiple range requests"
    );
}

#[tokio::test]
async fn pause_then_resume_continues_from_checkpoint() {
    // 20 ms per 16 KB chunk: each of 4 connections needs ~1.3 s for 4 MB. Wait
    // 900 ms before pausing so at least one checkpoint is written even on a
    // loaded machine, while the transfer is still clearly in flight.
    let server = TestServer::start(4 * 1024 * 1024, true, 20, 0).await;
    let tmp = tempfile::tempdir().unwrap();
    let cfg = test_config(tmp.path(), 4);
    let store = Store::at(tmp.path().join("state"));
    let engine = Arc::new(Engine::new(cfg, store).unwrap());

    let dl = Download::new(server.url(), tmp.path().to_string_lossy().into(), "file.bin".into(), 4);
    let ctl = CancellationToken::new();

    let run1 = tokio::spawn(run_download(engine.clone(), dl, ctl.clone()));
    tokio::time::sleep(Duration::from_millis(900)).await;
    ctl.cancel();
    let (outcome, paused) = run1.await.unwrap();
    assert_eq!(outcome, Outcome::Paused);
    assert_eq!(paused.status, Status::Paused);
    assert!(paused.downloaded > 0, "expected some progress before pause");
    assert!(paused.downloaded < paused.size);

    let resumed = Store::at(tmp.path().join("state")).load(&paused.id).unwrap();
    assert!(resumed.downloaded > 0, "checkpoint must be persisted");

    let (outcome, done) = run_download(engine, resumed, CancellationToken::new()).await;
    assert_eq!(outcome, Outcome::Completed, "error: {:?}", done.error);
    assert_eq!(std::fs::read(done.final_path()).unwrap(), payload(4 * 1024 * 1024));
    assert!(
        done.segments.iter().all(|s| s.done == s.len()),
        "every segment must be complete"
    );
}

#[tokio::test]
async fn falls_back_when_server_ignores_ranges() {
    let server = TestServer::start(512 * 1024, false, 0, 0).await;
    let tmp = tempfile::tempdir().unwrap();
    let cfg = test_config(tmp.path(), 8);
    let store = Store::at(tmp.path().join("state"));
    let engine = Arc::new(Engine::new(cfg, store).unwrap());

    let dl = Download::new(server.url(), tmp.path().to_string_lossy().into(), "file.bin".into(), 8);
    let (outcome, dl) = run_download(engine, dl, CancellationToken::new()).await;

    assert_eq!(outcome, Outcome::Completed, "error: {:?}", dl.error);
    assert!(!dl.ranges_supported);
    assert_eq!(std::fs::read(dl.final_path()).unwrap(), payload(512 * 1024));
}

#[tokio::test]
async fn retries_transient_500s() {
    let server = TestServer::start(256 * 1024, true, 0, 4).await;
    let tmp = tempfile::tempdir().unwrap();
    let cfg = test_config(tmp.path(), 4);
    let store = Store::at(tmp.path().join("state"));
    let engine = Arc::new(Engine::new(cfg, store).unwrap());

    let dl = Download::new(server.url(), tmp.path().to_string_lossy().into(), "file.bin".into(), 4);
    let (outcome, dl) = run_download(engine, dl, CancellationToken::new()).await;

    assert_eq!(outcome, Outcome::Completed, "error: {:?}", dl.error);
    assert_eq!(std::fs::read(dl.final_path()).unwrap(), payload(256 * 1024));
}
