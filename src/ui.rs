//! Local mill: a localhost-only web UI over [`crate::pack`].

use std::io::{Cursor, ErrorKind};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::Instant;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::config::{
    Options, OutputFormat, Selection, TreeMode, default_exclude_globs, parse_size,
};
use crate::manifest;
use crate::pack;
use crate::pick;
use crate::store::{self, Mill, StoredResult};

const INDEX: &str = include_str!("../web/index.html");

#[derive(Clone)]
struct AppState {
    pick: fn() -> Result<Option<PathBuf>, String>,
    token: Arc<str>,
    origin: Arc<str>,
    mill: Arc<Mill>,
}

const TEST_TOKEN: &str = "test-token";
const TEST_ORIGIN: &str = "http://127.0.0.1:8747";

/// Axum router used by `pulp ui` and the HTTP tests.
pub fn router() -> Router {
    router_with(AppState {
        pick: pick::pick_folder,
        token: Arc::from(TEST_TOKEN),
        origin: Arc::from(TEST_ORIGIN),
        mill: Arc::new(Mill::new()),
    })
}

fn router_with(state: AppState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/fonts/{name}", get(font_file))
        .route("/api/health", get(health))
        .route("/api/scan", post(scan))
        .route("/api/pack", post(pack_dump))
        .route("/api/tree", post(tree_dump))
        .route("/api/preview", post(preview))
        .route("/api/render", post(render_dump))
        .route("/api/cancel", post(cancel_pack))
        .route("/api/artifact/{id}", get(artifact))
        .route("/api/browse", post(browse))
        .with_state(state)
}

/// Bind `127.0.0.1` and serve the mill. Never listens on other interfaces.
///
/// When `try_next` is set (default port), a busy port walks 8747..=8767
/// instead of failing with "Address already in use".
pub async fn serve(preferred: u16, try_next: bool, open_browser: bool) -> anyhow::Result<()> {
    let (listener, port) = bind_localhost(preferred, try_next).await?;
    let url = format!("http://127.0.0.1:{port}");
    if port != preferred {
        eprintln!("127.0.0.1:{preferred} is busy; mill on {url}");
    } else {
        eprintln!("pulp mill on {url}");
    }
    eprintln!("localhost only. nothing is uploaded.");
    let state = AppState {
        pick: pick::pick_folder,
        token: Arc::from(new_session_token()?),
        origin: Arc::from(url.as_str()),
        mill: Arc::new(Mill::new()),
    };
    if open_browser {
        let _ = opener::open(&url);
    }
    axum::serve(listener, router_with(state))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}

fn new_session_token() -> anyhow::Result<String> {
    let mut buf = [0u8; 16];
    getrandom::fill(&mut buf).map_err(|err| anyhow::anyhow!("secure random unavailable: {err}"))?;
    Ok(buf.iter().fold(String::with_capacity(32), |mut s, b| {
        s.push_str(&format!("{b:02x}"));
        s
    }))
}

fn origin_matches(got: &str, allowed: &str) -> bool {
    got == allowed
}

fn host_name_ok(host: &str) -> bool {
    let host = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    host.eq_ignore_ascii_case("127.0.0.1")
        || host.eq_ignore_ascii_case("localhost")
        || host.eq_ignore_ascii_case("::1")
}

fn header_host_ok(host: &str) -> bool {
    let host = host.trim();
    if let Some(inner) = host.strip_prefix('[') {
        let name = inner.split(']').next().unwrap_or("");
        return host_name_ok(name);
    }
    let name = match host.rsplit_once(':') {
        Some((name, port)) if port.chars().all(|c| c.is_ascii_digit()) => name,
        _ => host,
    };
    host_name_ok(name)
}

fn authorize(state: &AppState, headers: &HeaderMap) -> Result<(), ApiError> {
    if let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) {
        if !origin_matches(origin, state.origin.as_ref()) {
            return Err(ApiError {
                status: StatusCode::FORBIDDEN,
                message: "bad origin".into(),
            });
        }
    }
    if let Some(host) = headers.get(header::HOST).and_then(|v| v.to_str().ok()) {
        if !header_host_ok(host) {
            return Err(ApiError {
                status: StatusCode::FORBIDDEN,
                message: "bad host".into(),
            });
        }
        if let Ok(uri) = state.origin.parse::<axum::http::Uri>() {
            let expected = match uri.port_u16() {
                Some(port) => format!("{}:{port}", uri.host().unwrap_or("127.0.0.1")),
                None => uri.host().unwrap_or("127.0.0.1").to_string(),
            };
            if host != expected && host != uri.host().unwrap_or("") {
                return Err(ApiError {
                    status: StatusCode::FORBIDDEN,
                    message: "bad host".into(),
                });
            }
        }
    }
    let got = headers
        .get("x-pulp-token")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if got != state.token.as_ref() {
        return Err(ApiError {
            status: StatusCode::UNAUTHORIZED,
            message: "bad session".into(),
        });
    }
    Ok(())
}

async fn bind_localhost(
    preferred: u16,
    try_next: bool,
) -> anyhow::Result<(tokio::net::TcpListener, u16)> {
    let last = if try_next {
        preferred.saturating_add(20)
    } else {
        preferred
    };
    for port in preferred..=last {
        let addr = SocketAddr::from(([127, 0, 0, 1], port));
        match tokio::net::TcpListener::bind(addr).await {
            Ok(listener) => return Ok((listener, port)),
            Err(err) if err.kind() == ErrorKind::AddrInUse => continue,
            Err(err) => return Err(err.into()),
        }
    }
    let holder = port_holder(preferred)
        .map(|h| format!(" (held by {h})"))
        .unwrap_or_default();
    Err(anyhow::anyhow!(
        "127.0.0.1:{preferred} is already in use{holder}. Stop that process or pass --port."
    ))
}

fn port_holder(port: u16) -> Option<String> {
    let output = Command::new("lsof")
        .args(["-nP", &format!("-iTCP:{port}"), "-sTCP:LISTEN", "-t"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let pid = String::from_utf8_lossy(&output.stdout)
        .lines()
        .next()?
        .trim()
        .to_string();
    if pid.is_empty() {
        return None;
    }
    Some(format!("pid {pid}"))
}

async fn index(State(state): State<AppState>) -> impl IntoResponse {
    let html = INDEX.replace("__PULP_TOKEN__", state.token.as_ref());
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        Html(html),
    )
}

async fn font_file(axum::extract::Path(name): axum::extract::Path<String>) -> Response {
    let bytes: &'static [u8] = match name.as_str() {
        "fraunces.woff2" => include_bytes!("../web/fonts/fraunces.woff2"),
        "plex-sans.woff2" => include_bytes!("../web/fonts/plex-sans.woff2"),
        "plex-mono-400.woff2" => include_bytes!("../web/fonts/plex-mono-400.woff2"),
        "plex-mono-500.woff2" => include_bytes!("../web/fonts/plex-mono-500.woff2"),
        _ => {
            return (StatusCode::NOT_FOUND, "unknown font").into_response();
        }
    };
    (
        [
            (header::CONTENT_TYPE, "font/woff2"),
            (header::CACHE_CONTROL, "public, max-age=31536000, immutable"),
        ],
        bytes,
    )
        .into_response()
}

async fn health() -> &'static str {
    "ok"
}

#[derive(Debug, Clone, Deserialize)]
struct ScanRequest {
    path: String,
    #[serde(default)]
    hidden: bool,
    #[serde(default = "default_true")]
    gitignore: bool,
    #[serde(default)]
    archives: bool,
    #[serde(default)]
    no_default_excludes: bool,
    #[serde(default)]
    exclude: Vec<String>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Serialize)]
struct ScanResponse {
    root: String,
    files: Vec<FileEntry>,
    file_count: usize,
    bytes: u64,
    truncated: bool,
    manifest_id: String,
}

#[derive(Debug, Serialize)]
struct FileEntry {
    id: String,
    relative: String,
    size: u64,
    kind: &'static str,
    language: &'static str,
    default_on: bool,
    oversized: bool,
}

#[derive(Debug, Clone, Deserialize)]
struct PackRequest {
    path: String,
    #[serde(default)]
    manifest_id: String,
    #[serde(default)]
    result_id: String,
    #[serde(default)]
    selected: Vec<String>,
    #[serde(default)]
    format: String,
    #[serde(default)]
    hidden: bool,
    #[serde(default = "default_true")]
    gitignore: bool,
    #[serde(default)]
    archives: bool,
    #[serde(default)]
    binaries: bool,
    #[serde(default)]
    notebook_outputs: bool,
    #[serde(default)]
    source: bool,
    #[serde(default)]
    no_tree: bool,
    #[serde(default)]
    no_default_excludes: bool,
    #[serde(default)]
    exclude: Vec<String>,
    #[serde(default)]
    max_file_size: Option<String>,
}

#[derive(Debug, Serialize)]
struct PackResponse {
    dump: String,
    filename: String,
    format: String,
    files_extracted: usize,
    files_skipped: usize,
    tokens_est: usize,
    chars_emitted: usize,
    dump_bytes: usize,
    elapsed_ms: u128,
    truncated: bool,
    cancelled: bool,
    preview_truncated: bool,
    manifest_id: String,
    result_id: String,
    outcomes: Vec<FileOutcome>,
}

#[derive(Debug, Serialize)]
struct FileOutcome {
    id: String,
    relative: String,
    status: &'static str,
    message: String,
}

#[derive(Debug, Serialize)]
struct PreviewResponse {
    id: String,
    relative: String,
    text: String,
    status: &'static str,
    message: String,
    truncated: bool,
    kind: &'static str,
    language: &'static str,
    size: u64,
}

#[derive(Debug, Serialize)]
struct TreeResponse {
    tree: String,
    filename: String,
    format: String,
}

#[derive(Debug, Serialize)]
struct BrowseResponse {
    path: Option<String>,
    cancelled: bool,
}

#[derive(Debug, Serialize)]
struct ErrorBody {
    error: String,
}

struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn bad(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
        }
    }

    fn busy() -> Self {
        Self {
            status: StatusCode::TOO_MANY_REQUESTS,
            message: "mill is busy".into(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = Json(ErrorBody {
            error: self.message,
        });
        (self.status, body).into_response()
    }
}

async fn scan(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<ScanRequest>,
) -> Result<Json<ScanResponse>, ApiError> {
    authorize(&state, &headers)?;
    let mill = Arc::clone(&state.mill);
    tokio::task::spawn_blocking(move || scan_sync(req, &mill))
        .await
        .map_err(|err| ApiError::bad(err.to_string()))?
        .map(Json)
}

async fn browse(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<BrowseResponse>, ApiError> {
    authorize(&state, &headers)?;
    let pick = state.pick;
    let chosen = tokio::task::spawn_blocking(pick)
        .await
        .map_err(|err| ApiError::bad(err.to_string()))?
        .map_err(ApiError::bad)?;
    match chosen {
        Some(path) => Ok(Json(BrowseResponse {
            path: Some(path.display().to_string()),
            cancelled: false,
        })),
        None => Ok(Json(BrowseResponse {
            path: None,
            cancelled: true,
        })),
    }
}

async fn pack_dump(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<PackRequest>,
) -> Result<Json<PackResponse>, ApiError> {
    authorize(&state, &headers)?;
    if !state.mill.try_begin_pack() {
        return Err(ApiError::busy());
    }
    let mill = Arc::clone(&state.mill);
    let result = tokio::task::spawn_blocking(move || pack_sync(req, &mill))
        .await
        .map_err(|err| ApiError::bad(err.to_string()));
    state.mill.end_pack();
    result?.map(Json)
}

async fn render_dump(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<PackRequest>,
) -> Result<Json<PackResponse>, ApiError> {
    authorize(&state, &headers)?;
    let mill = Arc::clone(&state.mill);
    tokio::task::spawn_blocking(move || render_sync(req, &mill))
        .await
        .map_err(|err| ApiError::bad(err.to_string()))?
        .map(Json)
}

async fn cancel_pack(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<StatusCode, ApiError> {
    authorize(&state, &headers)?;
    state.mill.request_cancel();
    Ok(StatusCode::NO_CONTENT)
}

async fn artifact(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<String>,
    axum::extract::Query(query): axum::extract::Query<ArtifactQuery>,
) -> Result<Response, ApiError> {
    authorize(&state, &headers)?;
    let mill = Arc::clone(&state.mill);
    tokio::task::spawn_blocking(move || artifact_sync(&id, &query, &mill))
        .await
        .map_err(|err| ApiError::bad(err.to_string()))?
}

#[derive(Debug, Deserialize)]
struct ArtifactQuery {
    #[serde(default)]
    format: String,
    #[serde(default)]
    no_tree: bool,
}

async fn tree_dump(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<PackRequest>,
) -> Result<Json<TreeResponse>, ApiError> {
    authorize(&state, &headers)?;
    let mill = Arc::clone(&state.mill);
    tokio::task::spawn_blocking(move || tree_sync(req, &mill))
        .await
        .map_err(|err| ApiError::bad(err.to_string()))?
        .map(Json)
}

fn scan_sync(req: ScanRequest, mill: &Mill) -> Result<ScanResponse, ApiError> {
    let key = discovery_key(&req);
    let root = resolve_root(&req.path)?;
    let opts = Options {
        roots: vec![root.clone()],
        gitignore: req.gitignore,
        hidden: req.hidden,
        follow_archives: req.archives,
        exclude: merge_excludes(req.no_default_excludes, req.exclude),
        list_only: true,
        ..Options::default()
    };
    let manifest = manifest::scan_manifest(&opts).map_err(|err| ApiError::bad(err.to_string()))?;
    let files: Vec<FileEntry> = manifest
        .entries
        .iter()
        .map(|entry| FileEntry {
            id: entry.id.clone(),
            relative: entry.relative.clone(),
            size: entry.size,
            kind: entry.kind.as_str(),
            language: entry.language,
            default_on: entry.default_on,
            oversized: entry.oversized,
        })
        .collect();
    let truncated = manifest.truncated;
    let bytes = manifest.bytes;
    let manifest_id = mill.put_manifest(key, manifest);
    Ok(ScanResponse {
        root: root.display().to_string(),
        file_count: files.len(),
        bytes,
        files,
        truncated,
        manifest_id,
    })
}

fn pack_sync(req: PackRequest, mill: &Mill) -> Result<PackResponse, ApiError> {
    if req.selected.is_empty() {
        return Err(ApiError::bad("tick at least one file"));
    }
    let extract_key = extract_key(&req);
    let opts = options_from_pack(req.clone(), false, false)?;
    let (manifest_id, mut snapshot) = load_or_scan_manifest(&req, mill)?;
    if let Some(hit) = mill.find_result(&manifest_id, &extract_key) {
        return finish_pack(hit, &opts, Instant::now());
    }
    snapshot.entries.retain(|entry| {
        req.selected
            .iter()
            .any(|id| id == &entry.id || id == &entry.relative)
    });
    let packed = pack::pack_manifest(&snapshot, &opts, Some(&mill.cancel), Instant::now())
        .map_err(|err| ApiError::bad(err.to_string()))?;
    let stored = StoredResult {
        id: store::new_id(),
        manifest_id: manifest_id.clone(),
        extract_key,
        files: packed.files.clone(),
        stats: packed.stats.clone(),
    };
    mill.put_result(stored.clone());
    finish_pack(stored, &opts, Instant::now())
}

fn render_sync(req: PackRequest, mill: &Mill) -> Result<PackResponse, ApiError> {
    if req.result_id.is_empty() {
        return Err(ApiError::bad("missing result_id"));
    }
    let stored = mill
        .get_result(&req.result_id)
        .ok_or_else(|| ApiError::bad("unknown result"))?;
    let opts = options_from_pack(req, false, false)?;
    finish_pack(stored, &opts, Instant::now())
}

fn artifact_sync(id: &str, query: &ArtifactQuery, mill: &Mill) -> Result<Response, ApiError> {
    let stored = mill
        .get_result(id)
        .ok_or_else(|| ApiError::bad("unknown result"))?;
    let format = if query.format.trim().is_empty() {
        OutputFormat::Xml
    } else {
        OutputFormat::from_ext(&query.format)
            .ok_or_else(|| ApiError::bad(format!("unknown format {}", query.format)))?
    };
    let opts = Options {
        format,
        tree: if query.no_tree {
            TreeMode::None
        } else {
            TreeMode::Selected
        },
        ..Options::default()
    };
    let packed = packed_from_stored(&stored, &opts);
    let mut dump = Cursor::new(Vec::new());
    crate::render::write_all(&mut dump, &packed, &opts)
        .map_err(|err| ApiError::bad(err.to_string()))?;
    let body = dump.into_inner();
    let ext = format.extension();
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        "text/plain; charset=utf-8".parse().expect("content-type"),
    );
    headers.insert(
        header::CONTENT_DISPOSITION,
        format!("attachment; filename=\"pulp.{ext}\"")
            .parse()
            .expect("content-disposition"),
    );
    Ok((headers, body).into_response())
}

fn load_or_scan_manifest(
    req: &PackRequest,
    mill: &Mill,
) -> Result<(String, crate::manifest::ScanManifest), ApiError> {
    if !req.manifest_id.is_empty() {
        if let Some(stored) = mill.get_manifest(&req.manifest_id) {
            return Ok((stored.id, stored.manifest));
        }
    }
    let opts = options_from_pack(req.clone(), true, false)?;
    let manifest = manifest::scan_manifest(&opts).map_err(|err| ApiError::bad(err.to_string()))?;
    let id = mill.put_manifest(discovery_key_pack(req), manifest.clone());
    Ok((id, manifest))
}

fn finish_pack(
    stored: StoredResult,
    opts: &Options,
    start: Instant,
) -> Result<PackResponse, ApiError> {
    let packed = packed_from_stored(&stored, opts);
    let mut dump = Cursor::new(Vec::new());
    crate::render::write_all(&mut dump, &packed, opts)
        .map_err(|err| ApiError::bad(err.to_string()))?;
    let dump =
        String::from_utf8(dump.into_inner()).map_err(|err| ApiError::bad(err.to_string()))?;
    let dump_bytes = dump.len();
    let (dump, preview_truncated) = store::cap_preview(&dump);
    let ext = opts.format.extension();
    let outcomes = packed
        .files
        .iter()
        .map(|file| FileOutcome {
            id: file.id.clone(),
            relative: file.relative.clone(),
            status: file.status.as_str(),
            message: file.status.message(file.size),
        })
        .collect();
    Ok(PackResponse {
        dump,
        dump_bytes,
        filename: format!("pulp.{ext}"),
        format: ext.to_string(),
        files_extracted: packed.stats.files_extracted,
        files_skipped: packed.stats.files_skipped,
        tokens_est: packed.stats.tokens_est,
        chars_emitted: packed.stats.chars_emitted,
        elapsed_ms: start.elapsed().as_millis(),
        truncated: packed.stats.truncated,
        cancelled: packed.stats.cancelled,
        preview_truncated,
        manifest_id: stored.manifest_id,
        result_id: stored.id,
        outcomes,
    })
}

fn packed_from_stored(stored: &StoredResult, opts: &Options) -> pack::Packed {
    let tree = if opts.tree == TreeMode::None {
        String::new()
    } else {
        let paths: Vec<String> = stored
            .files
            .iter()
            .filter(|f| f.status == pack::FileStatus::Extracted)
            .map(|f| f.relative.clone())
            .collect();
        crate::tree::render_tree(&pack::tree_label(&opts.roots), &paths)
    };
    pack::Packed {
        files: stored.files.clone(),
        tree,
        stats: stored.stats.clone(),
    }
}

fn discovery_key(req: &ScanRequest) -> String {
    format!(
        "{}|{}|{}|{}|{:?}",
        req.path, req.hidden, req.gitignore, req.archives, req.exclude
    )
}

fn discovery_key_pack(req: &PackRequest) -> String {
    format!(
        "{}|{}|{}|{}|{:?}",
        req.path, req.hidden, req.gitignore, req.archives, req.exclude
    )
}

fn extract_key(req: &PackRequest) -> String {
    let mut ids = req.selected.clone();
    ids.sort();
    format!(
        "{}|{}|{}|{}|{}",
        ids.join(","),
        req.source,
        req.notebook_outputs,
        req.archives,
        req.binaries
    )
}

async fn preview(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<PackRequest>,
) -> Result<Json<PreviewResponse>, ApiError> {
    authorize(&state, &headers)?;
    tokio::task::spawn_blocking(move || preview_sync(req))
        .await
        .map_err(|err| ApiError::bad(err.to_string()))?
        .map(Json)
}

fn preview_sync(req: PackRequest) -> Result<PreviewResponse, ApiError> {
    if req.selected.len() != 1 {
        return Err(ApiError::bad("preview one file"));
    }
    let opts = options_from_pack(req, false, false)?;
    let packed = pack::pack(&opts).map_err(|err| ApiError::bad(err.to_string()))?;
    let file = packed
        .files
        .first()
        .ok_or_else(|| ApiError::bad("file not found"))?;
    const CAP: usize = 16 * 1024;
    let truncated = file.text.len() > CAP;
    let text = if truncated {
        let mut end = CAP;
        while end > 0 && !file.text.is_char_boundary(end) {
            end -= 1;
        }
        file.text[..end].to_string()
    } else {
        file.text.clone()
    };
    Ok(PreviewResponse {
        id: file.id.clone(),
        relative: file.relative.clone(),
        status: file.status.as_str(),
        message: file.status.message(file.size),
        kind: file.kind.as_str(),
        language: crate::language_label(std::path::Path::new(&file.relative)),
        size: file.size,
        truncated,
        text,
    })
}

fn tree_sync(req: PackRequest, mill: &Mill) -> Result<TreeResponse, ApiError> {
    if req.selected.is_empty() {
        return Err(ApiError::bad("tick at least one file"));
    }
    let opts = options_from_pack(req.clone(), true, true)?;
    let (_, mut manifest) = load_or_scan_manifest(&req, mill)?;
    manifest.entries.retain(|entry| {
        req.selected
            .iter()
            .any(|id| id == &entry.id || id == &entry.relative)
    });
    let paths: Vec<String> = manifest
        .entries
        .iter()
        .map(|entry| entry.relative.clone())
        .collect();
    let packed = pack::Packed {
        files: Vec::new(),
        tree: crate::tree::render_tree(&pack::tree_label(&opts.roots), &paths),
        stats: pack::Stats {
            truncated: manifest.truncated,
            ..pack::Stats::default()
        },
    };
    let mut dump = Cursor::new(Vec::new());
    crate::render::write_tree(&mut dump, &packed, &opts)
        .map_err(|err| ApiError::bad(err.to_string()))?;
    let tree =
        String::from_utf8(dump.into_inner()).map_err(|err| ApiError::bad(err.to_string()))?;
    let ext = opts.format.extension();
    Ok(TreeResponse {
        tree,
        filename: format!("pulp-tree.{ext}"),
        format: ext.to_string(),
    })
}

fn options_from_pack(
    req: PackRequest,
    list_only: bool,
    force_tree: bool,
) -> Result<Options, ApiError> {
    let root = resolve_root(&req.path)?;
    let format = if req.format.trim().is_empty() {
        OutputFormat::Xml
    } else {
        OutputFormat::from_ext(&req.format)
            .ok_or_else(|| ApiError::bad(format!("unknown format {}", req.format)))?
    };
    let max_file_size = match req.max_file_size.as_deref() {
        None | Some("") => Options::default().max_file_size,
        Some(s) => parse_size(s).map_err(ApiError::bad)?,
    };
    let tree = if force_tree {
        TreeMode::Selected
    } else if req.no_tree {
        TreeMode::None
    } else {
        TreeMode::Selected
    };
    Ok(Options {
        roots: vec![root],
        gitignore: req.gitignore,
        hidden: req.hidden,
        follow_archives: req.archives,
        skip_binaries: !req.binaries,
        notebook_outputs: req.notebook_outputs,
        source_mode: req.source,
        tree,
        format,
        selection: Selection::Only(req.selected),
        exclude: merge_excludes(req.no_default_excludes, req.exclude),
        max_file_size,
        list_only,
        ..Options::default()
    })
}

fn merge_excludes(no_defaults: bool, extra: Vec<String>) -> Vec<String> {
    let mut exclude = if no_defaults {
        Vec::new()
    } else {
        default_exclude_globs()
    };
    exclude.extend(extra);
    exclude
}

fn resolve_root(raw: &str) -> Result<PathBuf, ApiError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(ApiError::bad("path is empty"));
    }
    let expanded = expand_tilde(trimmed);
    let path = if expanded.is_absolute() {
        expanded
    } else {
        std::env::current_dir()
            .map_err(|err| ApiError::bad(err.to_string()))?
            .join(expanded)
    };
    let canonical = path.canonicalize().unwrap_or(path);
    if !canonical.exists() {
        return Err(ApiError::bad(format!(
            "{} does not exist",
            canonical.display()
        )));
    }
    Ok(canonical)
}

fn expand_tilde(raw: &str) -> PathBuf {
    if raw == "~" {
        return home_dir().unwrap_or_else(|| PathBuf::from(raw));
    }
    if let Some(rest) = raw.strip_prefix("~/") {
        if let Some(home) = home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(raw)
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn testdata() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata")
    }

    async fn post_json(uri: &str, body: serde_json::Value) -> (StatusCode, serde_json::Value) {
        let response = router()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(uri)
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::HOST, "127.0.0.1:8747")
                    .header(header::ORIGIN, "http://127.0.0.1:8747")
                    .header("x-pulp-token", TEST_TOKEN)
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        (status, json)
    }

    #[tokio::test]
    async fn test_font_with_fraunces_returns_woff2() {
        let response = router()
            .oneshot(
                Request::builder()
                    .uri("/fonts/fraunces.woff2")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE).unwrap(),
            "font/woff2"
        );
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        assert!(bytes.len() > 1000);
    }

    #[tokio::test]
    async fn test_health_returns_ok() {
        let response = router()
            .oneshot(
                Request::builder()
                    .uri("/api/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_index_returns_html_mill() {
        let response = router()
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let html = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(html.contains("pulp mill"));
        assert!(html.contains("localhost"));
        assert!(html.contains("Fraunces"));
        assert!(html.contains("/fonts/fraunces.woff2"));
        assert!(html.contains(TEST_TOKEN));
        assert!(!html.contains("__PULP_TOKEN__"));
    }

    #[tokio::test]
    async fn test_scan_with_testdata_returns_lean_and_rust() {
        let path = testdata().display().to_string();
        let (status, json) = post_json(
            "/api/scan",
            serde_json::json!({ "path": path, "gitignore": false }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let names: Vec<&str> = json["files"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|f| f["relative"].as_str())
            .collect();
        assert!(names.iter().any(|n| n.ends_with("hello.rs")), "{names:?}");
        assert!(names.iter().any(|n| n.ends_with("Hello.lean")), "{names:?}");
        let lean = json["files"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["relative"].as_str() == Some("Hello.lean"))
            .unwrap();
        assert_eq!(lean["language"].as_str(), Some("lean"));
        assert_eq!(lean["kind"].as_str(), Some("text"));
        let rust = json["files"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["relative"].as_str() == Some("hello.rs"))
            .unwrap();
        assert_eq!(rust["language"].as_str(), Some("rust"));
        assert_eq!(rust["kind"].as_str(), Some("text"));
    }

    #[tokio::test]
    async fn test_pack_with_md_format_returns_markdown_dump() {
        let path = testdata().display().to_string();
        let (status, json) = post_json(
            "/api/pack",
            serde_json::json!({
                "path": path,
                "format": "md",
                "gitignore": false,
                "selected": ["hello.rs", "Hello.lean"]
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{json}");
        let dump = json["dump"].as_str().unwrap();
        assert!(
            dump.contains("## hello.rs") || dump.contains("hello.rs"),
            "{dump}"
        );
        assert!(dump.contains("```"), "{dump}");
        assert_eq!(json["filename"].as_str(), Some("pulp.md"));
        assert!(json["tokens_est"].as_u64().unwrap() > 0);
        assert!(json["outcomes"].as_array().unwrap().iter().any(|o| {
            o["relative"].as_str() == Some("hello.rs") && o["status"].as_str() == Some("extracted")
        }));
        assert!(json["result_id"].as_str().unwrap().len() > 4);
        assert!(json["manifest_id"].as_str().unwrap().len() > 4);
    }

    #[tokio::test]
    async fn test_scan_returns_manifest_id() {
        let path = testdata().display().to_string();
        let (status, json) = post_json(
            "/api/scan",
            serde_json::json!({ "path": path, "gitignore": false }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{json}");
        assert!(json["manifest_id"].as_str().unwrap().len() > 4);
    }

    #[tokio::test]
    async fn test_pack_busy_returns_too_many_requests() {
        let mill = Arc::new(Mill::new());
        assert!(mill.try_begin_pack());
        let app = router_with(AppState {
            pick: pick::pick_folder,
            token: Arc::from(TEST_TOKEN),
            origin: Arc::from(TEST_ORIGIN),
            mill,
        });
        let path = testdata().display().to_string();
        let body = serde_json::json!({
            "path": path,
            "format": "txt",
            "gitignore": false,
            "selected": ["hello.rs"]
        });
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/pack")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::HOST, "127.0.0.1:8747")
                    .header(header::ORIGIN, "http://127.0.0.1:8747")
                    .header("x-pulp-token", TEST_TOKEN)
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    }

    #[tokio::test]
    async fn test_render_with_result_id_returns_xml_without_new_scan() {
        let app = router();
        let path = testdata().display().to_string();
        let pack_body = serde_json::json!({
            "path": path,
            "format": "txt",
            "gitignore": false,
            "selected": ["hello.rs"]
        });
        let packed_res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/pack")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::HOST, "127.0.0.1:8747")
                    .header(header::ORIGIN, "http://127.0.0.1:8747")
                    .header("x-pulp-token", TEST_TOKEN)
                    .body(Body::from(pack_body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(packed_res.status(), StatusCode::OK);
        let packed_bytes = packed_res.into_body().collect().await.unwrap().to_bytes();
        let packed: serde_json::Value = serde_json::from_slice(&packed_bytes).unwrap();
        let result_id = packed["result_id"].as_str().unwrap().to_string();
        let render_body = serde_json::json!({
            "path": path,
            "format": "xml",
            "result_id": result_id,
            "selected": ["hello.rs"]
        });
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/render")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::HOST, "127.0.0.1:8747")
                    .header(header::ORIGIN, "http://127.0.0.1:8747")
                    .header("x-pulp-token", TEST_TOKEN)
                    .body(Body::from(render_body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(
            json["dump"].as_str().unwrap().contains("<documents>"),
            "{json}"
        );
        assert_eq!(json["result_id"].as_str(), Some(result_id.as_str()));
    }

    #[tokio::test]
    async fn test_preview_with_hello_rs_returns_source() {
        let path = testdata().display().to_string();
        let (status, json) = post_json(
            "/api/preview",
            serde_json::json!({
                "path": path,
                "gitignore": false,
                "selected": ["hello.rs"]
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{json}");
        assert!(json["text"].as_str().unwrap().contains("fn hello"));
        assert_eq!(json["relative"].as_str(), Some("hello.rs"));
        assert_eq!(json["status"].as_str(), Some("extracted"));
    }

    #[tokio::test]
    async fn test_pack_with_empty_format_returns_xml_dump() {
        let path = testdata().display().to_string();
        let (status, json) = post_json(
            "/api/pack",
            serde_json::json!({
                "path": path,
                "gitignore": false,
                "selected": ["hello.rs"]
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{json}");
        let dump = json["dump"].as_str().unwrap();
        assert!(dump.contains("<documents>"), "{dump}");
        assert_eq!(json["filename"].as_str(), Some("pulp.xml"));
        assert_eq!(json["format"].as_str(), Some("xml"));
    }

    fn stub_pick_testdata() -> Result<Option<PathBuf>, String> {
        Ok(Some(testdata()))
    }

    fn stub_pick_cancel() -> Result<Option<PathBuf>, String> {
        Ok(None)
    }

    #[tokio::test]
    async fn test_browse_with_stub_picker_returns_testdata_path() {
        let app = router_with(AppState {
            pick: stub_pick_testdata,
            token: Arc::from(TEST_TOKEN),
            origin: Arc::from(TEST_ORIGIN),
            mill: Arc::new(Mill::new()),
        });
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/browse")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::HOST, "127.0.0.1:8747")
                    .header("x-pulp-token", TEST_TOKEN)
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["cancelled"], false);
        let path = json["path"].as_str().unwrap();
        assert!(path.ends_with("testdata"), "{path}");
    }

    #[tokio::test]
    async fn test_browse_with_cancel_returns_cancelled() {
        let app = router_with(AppState {
            pick: stub_pick_cancel,
            token: Arc::from(TEST_TOKEN),
            origin: Arc::from(TEST_ORIGIN),
            mill: Arc::new(Mill::new()),
        });
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/browse")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::HOST, "127.0.0.1:8747")
                    .header("x-pulp-token", TEST_TOKEN)
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["cancelled"], true);
        assert!(json["path"].is_null());
    }

    #[tokio::test]
    async fn test_index_contains_browse_button() {
        let response = router()
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let html = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(html.contains("id=\"browse\""));
        assert!(html.contains("file manager"));
        assert!(html.contains("Copy tree"));
        assert!(html.contains("id=\"types\""));
        assert!(html.contains("id=\"stale\""));
        assert!(html.contains("x-pulp-token"));
        assert!(html.contains("id=\"settings\""));
        assert!(html.contains("class=\"info\""));
        assert!(html.contains("data-tip"));
        assert!(html.contains("data-fmt=\"xml\" class=\"on\""));
        assert!(html.contains("<option value=\"xml\" selected>"));
        assert!(html.contains("id=\"github\""));
        assert!(html.contains("https://github.com/BeeGass/pulp"));
        assert!(html.contains("Preserve source"));
        assert!(html.contains("Readable text"));
        assert!(html.contains("Select matches"));
        assert!(html.contains("data-out=\"issues\""));
        assert!(html.contains("/api/preview"));
        assert!(html.contains("/api/render"));
        assert!(html.contains("/api/cancel"));
        assert!(html.contains("/api/artifact/"));
    }

    #[test]
    fn test_origin_matches_with_exact_origin_returns_true() {
        assert!(origin_matches(
            "http://127.0.0.1:8747",
            "http://127.0.0.1:8747"
        ));
        assert!(!origin_matches(
            "http://127.0.0.1:9000",
            "http://127.0.0.1:8747"
        ));
        assert!(!origin_matches(
            "http://localhost:8747",
            "http://127.0.0.1:8747"
        ));
        assert!(!origin_matches(
            "http://127.0.0.1.evil.test",
            "http://127.0.0.1:8747"
        ));
    }

    #[test]
    fn test_header_host_ok_with_port_returns_true() {
        assert!(header_host_ok("127.0.0.1:8747"));
        assert!(header_host_ok("localhost"));
        assert!(header_host_ok("[::1]:8747"));
        assert!(!header_host_ok("example.com"));
        assert!(!header_host_ok("127.0.0.1.evil.test"));
    }

    #[tokio::test]
    async fn test_scan_with_missing_token_returns_unauthorized() {
        let response = router()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/scan")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"path":"."}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn test_scan_with_wrong_token_returns_unauthorized() {
        let response = router()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/scan")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header("x-pulp-token", "nope")
                    .body(Body::from(r#"{"path":"."}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn test_scan_with_bad_origin_returns_forbidden() {
        let response = router()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/scan")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::ORIGIN, "http://127.0.0.1.evil.test")
                    .header("x-pulp-token", TEST_TOKEN)
                    .body(Body::from(r#"{"path":"."}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn test_scan_with_bad_host_returns_forbidden() {
        let response = router()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/scan")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::HOST, "example.com")
                    .header("x-pulp-token", TEST_TOKEN)
                    .body(Body::from(r#"{"path":"."}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn test_tree_with_md_format_returns_fenced_tree_only() {
        let path = testdata().display().to_string();
        let (status, json) = post_json(
            "/api/tree",
            serde_json::json!({
                "path": path,
                "format": "md",
                "gitignore": false,
                "selected": ["hello.rs", "Hello.lean"]
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{json}");
        let tree = json["tree"].as_str().unwrap();
        assert!(tree.contains("# Directory structure"), "{tree}");
        assert!(tree.contains("```"), "{tree}");
        assert!(tree.contains("hello.rs"), "{tree}");
        assert!(!tree.contains("pub fn hello"), "{tree}");
        assert_eq!(json["filename"].as_str(), Some("pulp-tree.md"));
    }

    #[tokio::test]
    async fn test_pack_with_empty_selection_returns_error() {
        let path = testdata().display().to_string();
        let (status, json) = post_json(
            "/api/pack",
            serde_json::json!({ "path": path, "format": "txt", "selected": [] }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(json["error"].as_str().unwrap().contains("tick"));
    }

    #[tokio::test]
    async fn test_tree_with_empty_selection_returns_error() {
        let path = testdata().display().to_string();
        let (status, json) = post_json(
            "/api/tree",
            serde_json::json!({ "path": path, "format": "txt", "selected": [] }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(json["error"].as_str().unwrap().contains("tick"));
    }

    #[tokio::test]
    async fn test_scan_with_missing_path_returns_error() {
        let (status, json) = post_json(
            "/api/scan",
            serde_json::json!({ "path": "/no/such/pulp-ui-root" }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(json["error"].as_str().unwrap().contains("does not exist"));
    }
}
