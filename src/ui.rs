//! Local mill: a localhost-only web UI over [`crate::pack`].

use std::io::{Cursor, ErrorKind, Read};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

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

const INDEX: &str = include_str!("../web/index.html");

#[derive(Clone)]
struct AppState {
    pick: fn() -> Result<Option<PathBuf>, String>,
    token: Arc<str>,
}

const TEST_TOKEN: &str = "test-token";

/// Axum router used by `pulp ui` and the HTTP tests.
pub fn router() -> Router {
    router_with(AppState {
        pick: pick::pick_folder,
        token: Arc::from(TEST_TOKEN),
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
        token: Arc::from(new_session_token()),
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

fn new_session_token() -> String {
    let mut buf = [0u8; 16];
    let filled = std::fs::File::open("/dev/urandom")
        .ok()
        .and_then(|mut f| f.read_exact(&mut buf).ok());
    if filled.is_none() {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        buf[..8].copy_from_slice(&(nanos as u64).to_le_bytes());
        buf[8..12].copy_from_slice(&std::process::id().to_le_bytes());
    }
    buf.iter().fold(String::with_capacity(32), |mut s, b| {
        s.push_str(&format!("{b:02x}"));
        s
    })
}

fn origin_ok(origin: &str) -> bool {
    let Ok(uri) = origin.parse::<axum::http::Uri>() else {
        return false;
    };
    matches!(uri.scheme_str(), Some("http") | Some("https")) && uri.host().is_some_and(host_name_ok)
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
    if let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok())
        && !origin_ok(origin)
    {
        return Err(ApiError {
            status: StatusCode::FORBIDDEN,
            message: "bad origin".into(),
        });
    }
    if let Some(host) = headers.get(header::HOST).and_then(|v| v.to_str().ok())
        && !header_host_ok(host)
    {
        return Err(ApiError {
            status: StatusCode::FORBIDDEN,
            message: "bad host".into(),
        });
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

#[derive(Debug, Deserialize)]
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
}

#[derive(Debug, Serialize)]
struct FileEntry {
    relative: String,
    size: u64,
    kind: &'static str,
    default_on: bool,
    oversized: bool,
}

#[derive(Debug, Deserialize)]
struct PackRequest {
    path: String,
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
    elapsed_ms: u128,
    truncated: bool,
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
    tokio::task::spawn_blocking(move || scan_sync(req))
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
    tokio::task::spawn_blocking(move || pack_sync(req))
        .await
        .map_err(|err| ApiError::bad(err.to_string()))?
        .map(Json)
}

async fn tree_dump(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<PackRequest>,
) -> Result<Json<TreeResponse>, ApiError> {
    authorize(&state, &headers)?;
    tokio::task::spawn_blocking(move || tree_sync(req))
        .await
        .map_err(|err| ApiError::bad(err.to_string()))?
        .map(Json)
}

fn scan_sync(req: ScanRequest) -> Result<ScanResponse, ApiError> {
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
            relative: entry.relative.clone(),
            size: entry.size,
            kind: entry.language,
            default_on: entry.default_on,
            oversized: entry.oversized,
        })
        .collect();
    Ok(ScanResponse {
        root: root.display().to_string(),
        file_count: files.len(),
        bytes: manifest.bytes,
        files,
        truncated: manifest.truncated,
    })
}

fn pack_sync(req: PackRequest) -> Result<PackResponse, ApiError> {
    if req.selected.is_empty() {
        return Err(ApiError::bad("tick at least one file"));
    }
    let opts = options_from_pack(req, false, false)?;
    let packed = pack::pack(&opts).map_err(|err| ApiError::bad(err.to_string()))?;
    let format = opts.format;
    let mut dump = Cursor::new(Vec::new());
    crate::render::write_all(&mut dump, &packed, &opts)
        .map_err(|err| ApiError::bad(err.to_string()))?;
    let dump =
        String::from_utf8(dump.into_inner()).map_err(|err| ApiError::bad(err.to_string()))?;
    let ext = format.extension();
    Ok(PackResponse {
        dump,
        filename: format!("pulp.{ext}"),
        format: ext.to_string(),
        files_extracted: packed.stats.files_extracted,
        files_skipped: packed.stats.files_skipped,
        tokens_est: packed.stats.tokens_est,
        chars_emitted: packed.stats.chars_emitted,
        elapsed_ms: packed.stats.elapsed.as_millis(),
        truncated: packed.stats.truncated,
    })
}

fn tree_sync(req: PackRequest) -> Result<TreeResponse, ApiError> {
    if req.selected.is_empty() {
        return Err(ApiError::bad("tick at least one file"));
    }
    let opts = options_from_pack(req, true, true)?;
    let manifest = manifest::scan_manifest(&opts).map_err(|err| ApiError::bad(err.to_string()))?;
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
        OutputFormat::Plain
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
        assert_eq!(lean["kind"].as_str(), Some("lean"));
        let rust = json["files"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["relative"].as_str() == Some("hello.rs"))
            .unwrap();
        assert_eq!(rust["kind"].as_str(), Some("rust"));
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
    }

    #[test]
    fn test_origin_ok_with_loopback_returns_true() {
        assert!(origin_ok("http://127.0.0.1:8747"));
        assert!(origin_ok("http://localhost"));
        assert!(origin_ok("http://[::1]"));
        assert!(!origin_ok("http://127.0.0.1.evil.test"));
        assert!(!origin_ok("http://example.com"));
        assert!(!origin_ok("not a uri"));
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
