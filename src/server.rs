//! Local HTTP API + dashboard.
//!
//! Any local script can POST a URL here and TDM takes over the download; the
//! dashboard shows live progress and lets the user pause/resume.

use crate::config::Config;
use crate::engine::{Engine, Progress};
use crate::model::{Download, Status};
use crate::store::Store;
use crate::util::{category_for, sanitize_filename};
use anyhow::Result;
use axum::extract::{Path, State};
use axum::extract::DefaultBodyLimit;
use axum::http::StatusCode;
use axum::response::Html;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{Mutex, Semaphore};
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
struct AppState {
    cfg: Arc<Config>,
    store: Store,
    engine: Arc<Engine>,
    tasks: Arc<Mutex<HashMap<String, CancellationToken>>>,
    sem: Arc<Semaphore>,
}


#[derive(Deserialize)]
struct CreateReq {
    url: String,
    name: Option<String>,
    dir: Option<String>,
    connections: Option<u32>,
}


type ApiError = (StatusCode, String);

fn internal(err: anyhow::Error) -> ApiError {
    (StatusCode::INTERNAL_SERVER_ERROR, format!("{:#}", err))
}

pub async fn run(port: Option<u16>) -> Result<()> {
    let cfg = Arc::new(Config::load());
    let port = port.unwrap_or(cfg.api_port);
    let store = Store::new();
    store.ensure()?;
    // a previous process may have died mid-download: nothing is running now, so
    // mark those records paused instead of leaving them "downloading" forever
    for mut dl in store.load_all() {
        if dl.status.is_active() {
            dl.status = Status::Paused;
            let _ = store.save(&dl);
        }
    }
    let engine = Arc::new(Engine::new(cfg.clone(), store.clone())?);
    let state = AppState {
        sem: Arc::new(Semaphore::new(cfg.max_concurrent.max(1) as usize)),
        cfg,
        store,
        engine,
        tasks: Arc::new(Mutex::new(HashMap::new())),
    };

    let app = Router::new()
        .route("/", get(index))
        .route("/api/downloads", get(list_downloads).post(create))
        .route("/api/downloads/{id}", delete(remove_download))
        .route("/api/downloads/{id}/pause", post(pause_download))
        .route("/api/downloads/{id}/resume", post(resume_download))
        .layer(DefaultBodyLimit::max(64 * 1024 * 1024))
        .layer(tower_http::cors::CorsLayer::permissive())
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    use std::io::Write as _;
    let _ = writeln!(std::io::stdout(), "TDM dashboard: http://127.0.0.1:{}", port);
    let _ = writeln!(
        std::io::stdout(),
        "API: GET/POST http://127.0.0.1:{}/api/downloads",
        port
    );
    axum::serve(listener, app).await?;
    Ok(())
}

async fn index() -> Html<&'static str> {
    Html(DASHBOARD)
}

async fn list_downloads(State(state): State<AppState>) -> Json<Vec<Download>> {
    let mut items = state.store.load_all();
    items.sort_by_key(|d| std::cmp::Reverse(d.created_at));
    Json(items)
}

async fn create(
    State(state): State<AppState>,
    Json(req): Json<CreateReq>,
) -> Result<Json<Download>, ApiError> {
    if req.url.trim().is_empty() {
        return Err((StatusCode::BAD_REQUEST, "url is required".into()));
    }
    let name_locked = req.name.is_some();
    let name = match req.name {
        Some(name) => sanitize_filename(&name),
        None => sanitize_filename(
            &crate::http::filename_from_url(&req.url).unwrap_or_else(|| "download.bin".into()),
        ),
    };
    let mut dir = req.dir.unwrap_or_else(|| state.cfg.download_dir.clone());
    if state.cfg.categories && !name_locked {
        dir = format!("{}/{}", dir.trim_end_matches(['/', '\\']), category_for(&name));
    }
    let mut dl = Download::new(
        req.url,
        dir,
        name,
        req.connections.unwrap_or(state.cfg.connections),
    );
    dl.name_locked = name_locked;
    state.store.save(&dl).map_err(internal)?;
    spawn_download(state.clone(), dl.id.clone());
    Ok(Json(dl))
}


async fn pause_download(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let dl = state
        .store
        .resolve(&id)
        .ok_or((StatusCode::NOT_FOUND, "not found".to_string()))?;
    let token = state.tasks.lock().await.get(&dl.id).cloned();
    match token {
        Some(token) => {
            token.cancel();
            Ok(Json(json!({"ok": true})))
        }
        None => Err((StatusCode::CONFLICT, "download is not running".into())),
    }
}

async fn resume_download(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let dl = state
        .store
        .resolve(&id)
        .ok_or((StatusCode::NOT_FOUND, "not found".to_string()))?;
    if state.tasks.lock().await.contains_key(&dl.id) {
        return Err((StatusCode::CONFLICT, "already running".into()));
    }
    spawn_download(state.clone(), dl.id.clone());
    Ok(Json(json!({"ok": true})))
}

async fn remove_download(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let dl = state
        .store
        .resolve(&id)
        .ok_or((StatusCode::NOT_FOUND, "not found".to_string()))?;
    if let Some(token) = state.tasks.lock().await.remove(&dl.id) {
        token.cancel();
    }
    state.store.delete(&dl.id).map_err(internal)?;
    Ok(Json(json!({"ok": true})))
}

/// Queue worker: takes a semaphore permit, then runs the engine to completion.
fn spawn_download(state: AppState, id: String) {
    tokio::spawn(async move {
        let Ok(permit) = state.sem.clone().acquire_owned().await else {
            return;
        };
        let Some(mut dl) = state.store.load(&id) else {
            return;
        };
        if dl.status == Status::Completed {
            return;
        }
        let ctl = CancellationToken::new();
        state.tasks.lock().await.insert(id.clone(), ctl.clone());
        let progress = Progress::new();
        let _ = state.engine.run(&mut dl, ctl, progress).await;
        state.tasks.lock().await.remove(&id);
        drop(permit);
    });
}

const DASHBOARD: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>TDM</title>
<style>
  :root { color-scheme: light dark; }
  body { font: 14px/1.4 system-ui, sans-serif; margin: 24px auto; max-width: 1100px; padding: 0 16px; }
  h1 { font-size: 20px; }
  form { display: flex; gap: 8px; margin: 16px 0 24px; flex-wrap: wrap; }
  input[type=text] { flex: 1 1 420px; padding: 8px 10px; }
  input[type=number] { width: 80px; padding: 8px; }
  button { padding: 7px 12px; cursor: pointer; }
  table { width: 100%; border-collapse: collapse; }
  th, td { text-align: left; padding: 8px 6px; border-bottom: 1px solid #8884; vertical-align: middle; }
  td.num { text-align: right; font-variant-numeric: tabular-nums; }
  .bar { height: 8px; background: #8883; border-radius: 4px; overflow: hidden; min-width: 120px; }
  .bar > div { height: 100%; background: #2f81f7; }
  .s-completed { color: #2ea043; } .s-error { color: #d1242f; } .s-paused { color: #9a6700; }
  .muted { color: #888; }
</style>
</head>
<body>
<h1>TDM</h1>
<form id="add">
  <input type="text" id="url" placeholder="https://example.com/file.zip" required>
  <input type="number" id="conn" min="1" max="32" placeholder="conns">
  <button type="submit">Add URL</button>
</form>
<table>
  <thead><tr><th>Status</th><th>Name</th><th>Size</th><th>Progress</th><th></th></tr></thead>
  <tbody id="rows"></tbody>
</table>
<p class="muted" id="status"></p>
<script>
const tbody = document.getElementById('rows');
const fmt = n => n > 1073741824 ? (n/1073741824).toFixed(2)+' GB' :
  n > 1048576 ? (n/1048576).toFixed(2)+' MB' : n > 1024 ? (n/1024).toFixed(1)+' KB' : n+' B';

async function act(id, action) {
  await fetch(`/api/downloads/${id}/${action}`, {method:'POST'});
  refresh();
}
async function del(id) {
  if (!confirm('Remove this download from the list?')) return;
  await fetch(`/api/downloads/${id}`, {method:'DELETE'});
  refresh();
}
async function refresh() {
  try {
    const res = await fetch('/api/downloads');
    const items = await res.json();
    tbody.innerHTML = items.map(d => {
      const pct = d.size ? Math.min(100, d.downloaded / d.size * 100) : 0;
      const active = d.status === 'downloading' || d.status === 'connecting';
      const buttons = active
        ? `<button onclick="act('${d.id}','pause')">Pause</button>`
        : `<button onclick="act('${d.id}','resume')">Resume</button>`;
      return `<tr>
        <td class="s-${d.status}">${d.status}</td>
        <td>${d.file_name}<br><span class="muted">${d.category}</span></td>
        <td class="num">${fmt(d.size)}</td>
        <td><div class="bar"><div style="width:${pct}%"></div></div><span class="muted">${pct.toFixed(1)}%</span></td>
        <td>${buttons} <button onclick="del('${d.id}')">Remove</button></td>
      </tr>`;
    }).join('');
    document.getElementById('status').textContent = `${items.length} download(s) - updated ${new Date().toLocaleTimeString()}`;
  } catch (e) {
    document.getElementById('status').textContent = 'API unreachable';
  }
}
document.getElementById('add').addEventListener('submit', async (ev) => {
  ev.preventDefault();
  const url = document.getElementById('url').value.trim();
  const conns = parseInt(document.getElementById('conn').value) || undefined;
  if (!url) return;
  await fetch('/api/downloads', {
    method:'POST', headers:{'content-type':'application/json'},
    body: JSON.stringify({url, connections: conns})
  });
  document.getElementById('url').value = '';
  refresh();
});
refresh();
setInterval(refresh, 1000);
</script>
</body>
</html>
"#;

