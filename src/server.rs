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
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>TDM - The Download Manager</title>
<style>
  :root {
    --bg: #eef1f5;
    --panel: #f9fafc;
    --border: #c5cedb;
    --text: #1a2433;
    --dim: #657286;
    --blue: #0054a6;
    --blue-2: #2f6fd0;
    --green: #1f8a43;
    --red: #b42318;
    --amber: #9a6700;
  }
  * { box-sizing: border-box; }
  body {
    margin: 0;
    background: var(--bg);
    color: var(--text);
    font: 13px/1.45 Tahoma, "Segoe UI", sans-serif;
  }
  .top {
    background: linear-gradient(180deg, #2d6198 0%, #1d4d7b 55%, #163a5d 100%);
    color: #fff;
    border-bottom: 1px solid #0f2b45;
  }
  .top-inner {
    max-width: 1080px;
    margin: 0 auto;
    padding: 14px 22px;
    display: flex;
    align-items: center;
    gap: 12px;
  }
  .mark {
    width: 34px; height: 34px;
    display: grid; place-items: center;
    border-radius: 7px;
    background: #071727;
    color: #fff;
    font-size: 21px;
    font-weight: 700;
    border: 1px solid #5d8fbf;
  }
  .brand h1 { margin: 0; font-size: 17px; letter-spacing: .2px; }
  .brand span { color: #cfe2f7; font-size: 11px; }
  .conn {
    margin-left: auto;
    display: flex; align-items: center; gap: 6px;
    color: #dbe9f8; font-size: 11px;
  }
  .dot { width: 8px; height: 8px; border-radius: 50%; background: #c0392b; }
  .dot.on { background: #39c862; }
  main { max-width: 1080px; margin: 0 auto; padding: 18px 22px 28px; }
  .panel {
    background: #fff;
    border: 1px solid var(--border);
    border-radius: 4px;
    overflow: hidden;
    margin-bottom: 14px;
  }
  .panel-head {
    display: flex; align-items: center; gap: 8px;
    padding: 7px 11px;
    background: linear-gradient(180deg, #f4f8fd 0%, #dfe8f4 100%);
    border-bottom: 1px solid #c6d2e2;
    color: #1c3d63;
    font-weight: 700;
    font-size: 12px;
  }
  .panel-head .sub { margin-left: auto; color: var(--dim); font-weight: 400; font-size: 10.5px; }
  .panel-body { padding: 11px; }
  form { display: flex; gap: 8px; flex-wrap: wrap; }
  input[type=text], input[type=number] {
    padding: 7px 9px;
    border: 1px solid #8d9bad;
    border-radius: 2px;
    background: #fff;
    color: var(--text);
    font: inherit;
    outline: none;
  }
  input[type=text] { flex: 1 1 420px; }
  input[type=number] { width: 110px; }
  input:focus { border-color: var(--blue-2); box-shadow: 0 0 0 1px #b9d3ef; }
  button {
    padding: 7px 11px;
    border: 1px solid #8b98aa;
    border-radius: 3px;
    background: linear-gradient(180deg, #fff 0%, #e9eef6 100%);
    color: var(--text);
    font: inherit;
    cursor: pointer;
  }
  button:hover { background: linear-gradient(180deg, #fff 0%, #dce8f8 100%); }
  button.primary {
    background: linear-gradient(180deg, #3d82de 0%, var(--blue) 100%);
    border-color: #03427f;
    color: #fff;
    font-weight: 700;
  }
  button.danger { color: var(--red); }
  button.small { padding: 4px 8px; font-size: 11px; }
  .table-wrap { overflow-x: auto; }
  table { width: 100%; min-width: 760px; border-collapse: collapse; }
  th {
    text-align: left;
    padding: 7px 9px;
    background: #f1f5fa;
    border-bottom: 1px solid #c6d2e2;
    color: #233b56;
    font-size: 11px;
    white-space: nowrap;
  }
  td { padding: 8px 9px; border-bottom: 1px solid #e6eaf0; vertical-align: middle; }
  tbody tr:hover { background: #f5f9ff; }
  .file { font-weight: 600; }
  .file small { display: block; color: var(--dim); font-weight: 400; font-size: 10.5px; margin-top: 1px; }
  .badge {
    display: inline-block;
    min-width: 68px;
    text-align: center;
    padding: 2px 6px;
    border-radius: 10px;
    font-size: 10px;
    font-weight: 700;
    text-transform: capitalize;
    background: #edf1f7;
    color: var(--dim);
  }
  .s-downloading, .s-connecting { background: #e3eefb; color: var(--blue); }
  .s-completed { background: #e4f4e8; color: var(--green); }
  .s-paused { background: #fbf0d8; color: var(--amber); }
  .s-error { background: #fbe5e3; color: var(--red); }
  .num { text-align: right; font-variant-numeric: tabular-nums; white-space: nowrap; }
  .progress { min-width: 180px; }
  .bar { height: 9px; background: #e3e8ef; border-radius: 3px; overflow: hidden; }
  .bar > div { height: 100%; background: linear-gradient(90deg, #2f6fd0, #2fa7b8); }
  .progress small { display: block; color: var(--dim); font-size: 10px; margin-top: 3px; }
  .actions { white-space: nowrap; text-align: right; }
  .actions button { margin-left: 4px; }
  .empty { padding: 24px; text-align: center; color: var(--dim); }
  .foot { color: var(--dim); font-size: 10.5px; padding: 3px 2px; }
  .foot .api { float: right; font-family: Consolas, monospace; }
  @media (max-width: 700px) {
    main { padding: 12px; }
    .top-inner { padding: 12px; }
    .brand h1 { font-size: 15px; }
    form { flex-direction: column; }
    input[type=number] { width: 100%; }
  }
</style>
</head>
<body>
<header class="top">
  <div class="top-inner">
    <div class="mark">T</div>
    <div class="brand">
      <h1>TDM</h1>
      <span>The Download Manager &middot; local web interface</span>
    </div>
    <div class="conn"><span id="dot" class="dot"></span><span id="conn">connecting</span></div>
  </div>
</header>
<main>
  <section class="panel">
    <div class="panel-head">Add download <span class="sub">HTTP / HLS / DASH</span></div>
    <div class="panel-body">
      <form id="add">
        <input type="text" id="url" placeholder="https://example.com/file.zip" autocomplete="off" required>
        <input type="number" id="conn" min="1" max="32" placeholder="connections">
        <button class="primary" type="submit">Add URL</button>
      </form>
    </div>
  </section>

  <section class="panel">
    <div class="panel-head">Downloads <span class="sub" id="count">0 downloads</span></div>
    <div class="table-wrap">
      <table>
        <thead><tr><th>File</th><th>Size</th><th>Status</th><th>Progress</th><th class="actions">Actions</th></tr></thead>
        <tbody id="rows"></tbody>
      </table>
    </div>
  </section>

  <div class="foot"><span id="updated">Loading...</span><span class="api">/api/downloads</span></div>
</main>
<script>
const tbody = document.getElementById('rows');
const conn = document.getElementById('conn');
const dot = document.getElementById('dot');
const count = document.getElementById('count');
const updated = document.getElementById('updated');
const esc = (s) => String(s).replace(/[&<>"']/g, c => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c]));
const fmt = n => n > 1073741824 ? (n/1073741824).toFixed(2)+' GB' :
  n > 1048576 ? (n/1048576).toFixed(2)+' MB' : n > 1024 ? (n/1024).toFixed(1)+' KB' : n+' B';

async function act(id, action) {
  await fetch('/api/downloads/' + encodeURIComponent(id) + '/' + action, {method:'POST'});
  refresh();
}
async function del(id) {
  if (!confirm('Remove this download from the list?')) return;
  await fetch('/api/downloads/' + encodeURIComponent(id), {method:'DELETE'});
  refresh();
}
async function refresh() {
  try {
    const res = await fetch('/api/downloads', {cache:'no-store'});
    const items = await res.json();
    count.textContent = items.length + (items.length === 1 ? ' download' : ' downloads');
    if (!items.length) {
      tbody.innerHTML = '<tr><td colspan="5" class="empty">No downloads yet. Add a URL above.</td></tr>';
    } else {
      tbody.innerHTML = items.map(d => {
        const pct = d.size ? Math.min(100, d.downloaded / d.size * 100) : 0;
        const active = d.status === 'downloading' || d.status === 'connecting';
        const canResume = !active && d.status !== 'completed';
        const buttons = active
          ? '<button class="small" onclick="act(\'' + esc(d.id) + '\',\'pause\')">Pause</button>'
          : canResume
            ? '<button class="small" onclick="act(\'' + esc(d.id) + '\',\'resume\')">Resume</button>'
            : '';
        return '<tr>' +
          '<td class="file">' + esc(d.file_name) + '<small>' + esc(d.category || 'Other') + '</small></td>' +
          '<td class="num">' + fmt(d.size) + '</td>' +
          '<td><span class="badge s-' + esc(d.status) + '">' + esc(d.status) + '</span></td>' +
          '<td class="progress"><div class="bar"><div style="width:' + pct.toFixed(1) + '%"></div></div><small>' + pct.toFixed(1) + '%</small></td>' +
          '<td class="actions">' + buttons + '<button class="small danger" onclick="del(\'' + esc(d.id) + '\')">Remove</button></td>' +
          '</tr>';
      }).join('');
    }
    dot.className = 'dot on';
    conn.textContent = 'connected';
    updated.textContent = items.length + (items.length === 1 ? ' download' : ' downloads') + ' - updated ' + new Date().toLocaleTimeString();
  } catch (e) {
    dot.className = 'dot';
    conn.textContent = 'offline';
    updated.textContent = 'TDM API is unreachable';
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
setInterval(refresh, 1500);
</script>
</body>
</html>
"#;

