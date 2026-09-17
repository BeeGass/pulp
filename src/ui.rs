//! Local mill: a localhost-only web UI over [`crate::pack`].

use std::io::{Cursor, ErrorKind};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::Command;

use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::classify::{classify, is_default_selected};
use crate::config::{Options, OutputFormat, TreeMode, default_exclude_globs, parse_size};
use crate::pack;
use crate::pick;
use crate::walk;

const INDEX: &str = include_str!("../web/index.html");

#[derive(Clone, Copy)]
struct AppState {
    pick: fn() -> Result<Option<PathBuf>, String>,
}

/// Axum router used by `pulp ui` and the HTTP tests.
pub fn router() -> Router {
    router_with(AppState {
        pick: pick::pick_folder,
    })
}

fn router_with(state: AppState) -> Router {
    Router::new()
        .route("/", get(index))
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
    if open_browser {
        let _ = opener::open(&url);
    }
    axum::serve(listener, router())
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
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

async fn index() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        Html(INDEX),
    )
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
}

#[derive(Debug, Serialize)]
struct FileEntry {
    relative: String,
    size: u64,
    kind: &'static str,
    default_on: bool,
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
    State(_): State<AppState>,
    Json(req): Json<ScanRequest>,
) -> Result<Json<ScanResponse>, ApiError> {
    tokio::task::spawn_blocking(move || scan_sync(req))
        .await
        .map_err(|err| ApiError::bad(err.to_string()))?
        .map(Json)
}

async fn browse(State(state): State<AppState>) -> Result<Json<BrowseResponse>, ApiError> {
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
    State(_): State<AppState>,
    Json(req): Json<PackRequest>,
) -> Result<Json<PackResponse>, ApiError> {
    tokio::task::spawn_blocking(move || pack_sync(req))
        .await
        .map_err(|err| ApiError::bad(err.to_string()))?
        .map(Json)
}

async fn tree_dump(
    State(_): State<AppState>,
    Json(req): Json<PackRequest>,
) -> Result<Json<TreeResponse>, ApiError> {
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
    let walked = walk::collect(&opts).map_err(|err| ApiError::bad(err.to_string()))?;
    let files: Vec<FileEntry> = walked
        .iter()
        .map(|file| {
            let kind = classify(&file.absolute, None);
            FileEntry {
                relative: file.relative.clone(),
                size: file.size,
                kind: kind.as_str(),
                default_on: is_default_selected(&file.absolute, kind),
            }
        })
        .collect();
    let bytes = files.iter().map(|f| f.size).sum();
    Ok(ScanResponse {
        root: root.display().to_string(),
        file_count: files.len(),
        bytes,
        files,
    })
}

fn pack_sync(req: PackRequest) -> Result<PackResponse, ApiError> {
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
    })
}

fn tree_sync(req: PackRequest) -> Result<TreeResponse, ApiError> {
    if req.selected.is_empty() {
        return Err(ApiError::bad("tick at least one file"));
    }
    let opts = options_from_pack(req, true, true)?;
    let packed = pack::pack(&opts).map_err(|err| ApiError::bad(err.to_string()))?;
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
        tree,
        format,
        selected: req.selected,
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
        });
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/browse")
                    .header(header::CONTENT_TYPE, "application/json")
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
        });
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/browse")
                    .header(header::CONTENT_TYPE, "application/json")
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
