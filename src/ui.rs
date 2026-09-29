//! Local mill: a localhost-only web UI over [`crate::pack`].
//!
//! Who can use the mill:
//!
//! - It listens on `127.0.0.1` only, and every request must name
//!   `127.0.0.1:<port>` as its host, so a site that points its own domain at
//!   127.0.0.1 (DNS rebinding) is turned away before any route runs.
//! - The page opens only from the link `pulp ui` prints, which carries the
//!   session token, or from the one-time link it opens in the browser. Other
//!   users and programs that can reach the port get a locked page instead.
//! - Every API call must send the token in `x-pulp-token`. Calls from another
//!   origin or site are refused, and a CORS preflight gets no CORS headers.

use std::io::{Cursor, ErrorKind};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;

use crate::config::{Options, OutputFormat, Selection, SelectionFilter, TreeMode, parse_size};
use crate::manifest::{self, ManifestEntry, ScanManifest};
use crate::pack;
use crate::pick;
use crate::store::{self, JobGuard, Mill, PackJob, StoredManifest, StoredResult};

const INDEX: &str = include_str!("../web/index.html");
const PULP_CSS: &str = include_str!("../web/pulp.css");
const MILL_CSS: &str = include_str!("../web/mill.css");
const MILL_JS: &str = include_str!("../web/mill.js");

/// Scans, directory maps, redraws, and full downloads that may run at once.
/// Later ones wait for a slot rather than failing.
const WORK_SLOTS: usize = 4;

/// How long the one-time link that `pulp ui` opens in the browser stays valid.
const LAUNCH_TTL: Duration = Duration::from_secs(120);

#[derive(Clone)]
struct AppState {
    pick: fn() -> Result<Option<PathBuf>, String>,
    token: Arc<str>,
    origin: Arc<str>,
    mill: Arc<Mill>,
    sample: Arc<crate::sample::Scratch>,
    /// The one-time code in the link opened in the browser, until it is used.
    launch: Arc<Mutex<Option<Launch>>>,
    /// Set while the native folder dialog is open.
    picking: Arc<AtomicBool>,
    work: Arc<Semaphore>,
}

impl AppState {
    fn new(token: &str, origin: &str) -> Self {
        Self {
            pick: pick::pick_folder,
            token: Arc::from(token),
            origin: Arc::from(origin),
            mill: Arc::new(Mill::new()),
            sample: Arc::default(),
            launch: Arc::default(),
            picking: Arc::default(),
            work: Arc::new(Semaphore::new(WORK_SLOTS)),
        }
    }
}

/// A one-time page code and when it stops working.
struct Launch {
    code: String,
    until: Instant,
}

#[cfg(test)]
const TEST_TOKEN: &str = "test-token";
#[cfg(test)]
const TEST_ORIGIN: &str = "http://127.0.0.1:8747";

/// The mill's router with a fixed test session, for the HTTP tests.
#[cfg(test)]
fn router() -> Router {
    router_with(AppState::new(TEST_TOKEN, TEST_ORIGIN))
}

fn router_with(state: AppState) -> Router {
    // Every API route checks the session before its handler runs, before a
    // request body is even read.
    let api = Router::new()
        .route("/api/scan", post(scan))
        .route("/api/pack", post(pack_dump))
        .route("/api/tree", post(tree_dump))
        .route("/api/preview", post(preview))
        .route("/api/render", post(render_dump))
        .route("/api/cancel", post(cancel_pack))
        .route("/api/progress", get(progress))
        .route("/api/artifact/{id}", get(artifact))
        .route("/api/browse", post(browse))
        .route("/api/sample", post(sample))
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            require_session,
        ));
    Router::new()
        .route("/", get(index))
        .route("/pulp.css", get(pulp_css))
        .route("/mill.css", get(mill_css))
        .route("/mill.js", get(mill_js))
        .route("/fonts/{name}", get(font_file))
        .route("/api/health", get(health))
        .merge(api)
        .fallback(not_found)
        .layer(axum::middleware::from_fn_with_state(state.clone(), guard))
        .with_state(state)
}

/// Bind `127.0.0.1` and serve the mill. Never listens on other interfaces.
///
/// When `try_next` is set (default port), a busy port walks 8747..=8767
/// instead of failing with "Address already in use".
pub async fn serve(preferred: u16, try_next: bool, open_browser: bool) -> anyhow::Result<()> {
    let (listener, port) = bind_localhost(preferred, try_next).await?;
    let origin = format!("http://127.0.0.1:{port}");
    let token = new_secret()?;
    if preferred != 0 && port != preferred {
        eprintln!("127.0.0.1:{preferred} is busy; mill on {origin}");
    } else {
        eprintln!("pulp mill on {origin}");
    }
    // The link carries the session token: only someone who can read this
    // terminal can open the page.
    eprintln!("open {origin}/?token={token}");
    eprintln!("localhost only. nothing is uploaded.");
    let state = AppState::new(&token, &origin);
    let sample = Arc::clone(&state.sample);
    let mill = Arc::clone(&state.mill);
    if open_browser {
        // The browser gets a one-time code, not the token: a command line can
        // be visible to other users, and the code is spent on first use.
        let code = new_secret()?;
        *state.launch.lock().unwrap_or_else(PoisonError::into_inner) = Some(Launch {
            code: code.clone(),
            until: Instant::now() + LAUNCH_TTL,
        });
        let _ = opener::open(format!("{origin}/?launch={code}"));
    }
    let stopping = Arc::new(tokio::sync::Notify::new());
    let graceful = {
        let (mill, stopping) = (Arc::clone(&mill), Arc::clone(&stopping));
        async move {
            shutdown_signal().await;
            // A running pack stops between files instead of holding the exit.
            mill.request_cancel();
            stopping.notify_one();
        }
    };
    let server = axum::serve(listener, router_with(state)).with_graceful_shutdown(graceful);
    // Open requests get a short grace to finish; a second signal ends it early.
    let deadline = async {
        stopping.notified().await;
        tokio::select! {
            () = tokio::time::sleep(SHUTDOWN_GRACE) => {}
            () = shutdown_signal() => {}
        }
    };
    let served = tokio::select! {
        served = server.into_future() => served,
        () = deadline => Ok(()),
    };
    sample.remove();
    served?;
    Ok(())
}

/// How long open requests get to finish after a stop signal.
const SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

/// Resolve on Ctrl-C, and on SIGTERM or SIGHUP where they exist, so the mill
/// stops cleanly (and removes its sample folder) however its terminal ends.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let wait = |kind: SignalKind| async move {
            match signal(kind) {
                Ok(mut stream) => {
                    stream.recv().await;
                }
                Err(_) => std::future::pending::<()>().await,
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            () = wait(SignalKind::terminate()) => {}
            () = wait(SignalKind::hangup()) => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// 128 random bits as hex: the session token, one-time codes, and nonces.
fn new_secret() -> anyhow::Result<String> {
    let mut buf = [0u8; 16];
    getrandom::fill(&mut buf).map_err(|err| anyhow::anyhow!("secure random unavailable: {err}"))?;
    Ok(buf.iter().fold(String::with_capacity(32), |mut s, b| {
        s.push_str(&format!("{b:02x}"));
        s
    }))
}

/// Compare a presented secret with the real one without stopping at the
/// first differing byte, so response timing does not reveal a matching prefix.
fn same_secret(got: &[u8], want: &[u8]) -> bool {
    if got.len() != want.len() {
        return false;
    }
    let diff = got.iter().zip(want).fold(0u8, |acc, (a, b)| acc | (a ^ b));
    std::hint::black_box(diff) == 0
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

/// What a request's `Host` says about who sent it.
#[derive(Debug, PartialEq, Eq)]
enum HostVerdict {
    /// `127.0.0.1:<port>`, the mill's own address.
    Ours,
    /// Another name for this machine on the mill's port, such as `localhost`.
    Loopback,
    /// Anything else: another name (DNS rebinding), a missing or unreadable
    /// host, or a request line that names a different host.
    Foreign,
}

fn host_verdict(origin: &str, headers: &HeaderMap, uri: &axum::http::Uri) -> HostVerdict {
    let Some(host) = headers.get(header::HOST).and_then(|v| v.to_str().ok()) else {
        return HostVerdict::Foreign;
    };
    if uri
        .authority()
        .is_some_and(|authority| !authority.as_str().eq_ignore_ascii_case(host))
    {
        return HostVerdict::Foreign;
    }
    let ours = origin.strip_prefix("http://").unwrap_or(origin);
    if host.eq_ignore_ascii_case(ours) {
        return HostVerdict::Ours;
    }
    let port = ours.rsplit_once(':').map_or("", |(_, port)| port);
    let same_port = host
        .rsplit_once(':')
        .is_some_and(|(_, got)| !port.is_empty() && got == port);
    if same_port && header_host_ok(host) {
        HostVerdict::Loopback
    } else {
        HostVerdict::Foreign
    }
}

/// Runs around every route. Turns away requests for any host but the mill's
/// own, sends the page opened under `localhost` to `127.0.0.1` (where its
/// origin check passes), and adds the headers every response carries.
async fn guard(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let verdict = host_verdict(&state.origin, request.headers(), request.uri());
    let is_page =
        matches!(*request.method(), Method::GET | Method::HEAD) && request.uri().path() == "/";
    let mut response = match verdict {
        HostVerdict::Ours => next.run(request).await,
        HostVerdict::Loopback if is_page => {
            let query = request
                .uri()
                .query()
                .map(|q| format!("?{q}"))
                .unwrap_or_default();
            let target = format!("{}/{query}", state.origin);
            match HeaderValue::from_str(&target) {
                Ok(location) => (
                    StatusCode::TEMPORARY_REDIRECT,
                    [(header::LOCATION, location)],
                )
                    .into_response(),
                Err(_) => bad_host(),
            }
        }
        HostVerdict::Loopback | HostVerdict::Foreign => bad_host(),
    };
    harden(response.headers_mut());
    response
}

fn bad_host() -> Response {
    ApiError {
        status: StatusCode::FORBIDDEN,
        message: "bad host".into(),
    }
    .into_response()
}

/// A policy for responses that are never meant to run as a page.
const INERT_CSP: &str =
    "default-src 'none'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'";

/// Headers on every response: no MIME sniffing, no framing by other origins,
/// no referrer, no cross-origin embedding or window handles, and nothing kept
/// in the browser cache unless the route chose a cache policy itself.
fn harden(headers: &mut HeaderMap) {
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    // Only the mill's own origin may frame it; no other site can overlay it.
    headers.insert(
        header::X_FRAME_OPTIONS,
        HeaderValue::from_static("SAMEORIGIN"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        HeaderName::from_static("cross-origin-resource-policy"),
        HeaderValue::from_static("same-origin"),
    );
    headers.insert(
        HeaderName::from_static("cross-origin-opener-policy"),
        HeaderValue::from_static("same-origin"),
    );
    headers
        .entry(header::CACHE_CONTROL)
        .or_insert(HeaderValue::from_static("no-store"));
    headers
        .entry(header::CONTENT_SECURITY_POLICY)
        .or_insert(HeaderValue::from_static(INERT_CSP));
}

/// Runs before every API handler: refuses a request that [`authorize`] does
/// not accept.
async fn require_session(State(state): State<AppState>, request: Request, next: Next) -> Response {
    match authorize(&state, request.headers()) {
        Ok(()) => next.run(request).await,
        Err(err) => err.into_response(),
    }
}

/// API calls must come from the mill's own page: same origin, same-origin
/// fetch, and this session's token.
fn authorize(state: &AppState, headers: &HeaderMap) -> Result<(), ApiError> {
    if let Some(origin) = headers.get(header::ORIGIN) {
        // An origin that is not even text is not ours either.
        let ours = origin
            .to_str()
            .is_ok_and(|origin| origin_matches(origin, state.origin.as_ref()));
        if !ours {
            return Err(ApiError {
                status: StatusCode::FORBIDDEN,
                message: "bad origin".into(),
            });
        }
    }
    // Browsers name where a request came from. Only the page's own fetches
    // (same-origin) may reach the API; other sites on this machine, even other
    // ports of 127.0.0.1, are same-site at best.
    if let Some(site) = headers.get("sec-fetch-site") {
        if site.as_bytes() != b"same-origin" {
            return Err(ApiError {
                status: StatusCode::FORBIDDEN,
                message: "cross-site request".into(),
            });
        }
    }
    let got = headers
        .get("x-pulp-token")
        .map(HeaderValue::as_bytes)
        .unwrap_or_default();
    if !same_secret(got, state.token.as_bytes()) {
        return Err(ApiError {
            status: StatusCode::UNAUTHORIZED,
            message: "bad session".into(),
        });
    }
    Ok(())
}

/// Bind the first free port on `127.0.0.1` from `preferred` (up to 20 more
/// when `try_next` is set) and return the listener with the port it got. Port
/// 0 asks the system for any free port.
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
            Ok(listener) => {
                let bound = listener.local_addr()?.port();
                return Ok((listener, bound));
            }
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

#[derive(Debug, Default, Deserialize)]
struct PageQuery {
    token: Option<String>,
    launch: Option<String>,
}

/// The mill page, for a request that proves it came from `pulp ui`: the
/// printed link's `?token=`, or the one-time `?launch=` code, which redirects
/// to the token link. Anything else gets the locked page, so a local program
/// that can reach the port cannot read the token out of the page.
async fn index(
    State(state): State<AppState>,
    axum::extract::Query(query): axum::extract::Query<PageQuery>,
) -> Response {
    if let Some(code) = query.launch.as_deref() {
        if !take_launch(&state, code) {
            return locked_page();
        }
        let target = format!("/?token={}", state.token);
        return match HeaderValue::from_str(&target) {
            Ok(location) => (StatusCode::SEE_OTHER, [(header::LOCATION, location)]).into_response(),
            Err(_) => locked_page(),
        };
    }
    match query.token.as_deref() {
        Some(token) if same_secret(token.as_bytes(), state.token.as_bytes()) => mill_page(&state),
        _ => locked_page(),
    }
}

/// Spend the one-time code if `code` is it and it has not expired.
fn take_launch(state: &AppState, code: &str) -> bool {
    let mut slot = state.launch.lock().unwrap_or_else(PoisonError::into_inner);
    let valid = slot.as_ref().is_some_and(|launch| {
        Instant::now() <= launch.until && same_secret(code.as_bytes(), launch.code.as_bytes())
    });
    if valid {
        *slot = None;
    }
    valid
}

/// The mill shell with this session's token, and a policy that runs only the
/// page's own scripts: `/mill.js` and the inline module, which gets a fresh
/// nonce on every load.
fn mill_page(state: &AppState) -> Response {
    let Ok(nonce) = new_secret() else {
        return ApiError::internal("secure random unavailable for a page nonce").into_response();
    };
    let html = INDEX
        .replace("__PULP_TOKEN__", state.token.as_ref())
        .replace("__PULP_VERSION__", env!("CARGO_PKG_VERSION"))
        .replace("<script", &format!("<script nonce=\"{nonce}\""));
    let csp = format!(
        "default-src 'none'; script-src 'self' 'nonce-{nonce}'; style-src 'self' 'unsafe-inline'; \
         img-src 'self' data:; font-src 'self'; connect-src 'self'; base-uri 'none'; \
         form-action 'none'; frame-ancestors 'self'"
    );
    let Ok(csp) = HeaderValue::from_str(&csp) else {
        return ApiError::internal("page policy is not a header value").into_response();
    };
    (
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/html; charset=utf-8"),
            ),
            (header::CACHE_CONTROL, HeaderValue::from_static("no-store")),
            (header::CONTENT_SECURITY_POLICY, csp),
        ],
        Html(html),
    )
        .into_response()
}

/// Shown instead of the mill when a request cannot show it came from `pulp ui`.
const LOCKED_PAGE: &str = r#"<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <meta name="color-scheme" content="light dark">
  <title>pulp mill</title>
</head>
<body style="font: 15px/1.5 system-ui, sans-serif; max-width: 36rem; margin: 12vh auto; padding: 0 20px">
  <h1 style="font-size: 18px">Open the mill from its link</h1>
  <p>This address needs the link that <code>pulp ui</code> printed in its terminal. The link carries this session's key, so other programs on this machine cannot use the mill.</p>
  <p>If <code>pulp ui</code> has restarted, the old link no longer works: use the new one it printed.</p>
</body>
</html>
"#;

const LOCKED_CSP: &str = "default-src 'none'; style-src 'unsafe-inline'; base-uri 'none'; \
     form-action 'none'; frame-ancestors 'self'";

fn locked_page() -> Response {
    (
        StatusCode::FORBIDDEN,
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/html; charset=utf-8"),
            ),
            (header::CACHE_CONTROL, HeaderValue::from_static("no-store")),
            (
                header::CONTENT_SECURITY_POLICY,
                HeaderValue::from_static(LOCKED_CSP),
            ),
        ],
        Html(LOCKED_PAGE),
    )
        .into_response()
}

async fn font_file(axum::extract::Path(name): axum::extract::Path<String>) -> Response {
    let bytes: &'static [u8] = match name.as_str() {
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

async fn pulp_css() -> Response {
    asset(PULP_CSS, "text/css; charset=utf-8")
}

async fn mill_css() -> Response {
    asset(MILL_CSS, "text/css; charset=utf-8")
}

async fn mill_js() -> Response {
    asset(MILL_JS, "text/javascript; charset=utf-8")
}

/// Embedded UI assets change with the binary, so browsers revalidate each load.
fn asset(body: &'static str, content_type: &'static str) -> Response {
    (
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        body,
    )
        .into_response()
}

async fn health() -> &'static str {
    "ok"
}

async fn not_found() -> ApiError {
    ApiError::not_found("not found")
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
    /// The first few paths the walk could not read, relative to the root.
    warnings: Vec<String>,
    /// How many paths the walk could not read, shown or not.
    warning_count: usize,
}

/// Walk warnings a scan answer carries; the mill counts the rest.
const SCAN_WARNINGS_SHOWN: usize = 3;

#[derive(Debug, Serialize)]
struct FileEntry {
    id: String,
    relative: String,
    size: u64,
    kind: &'static str,
    language: String,
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
    extract_ms: u128,
    render_ms: u128,
    cache_hit: bool,
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
    kind: &'static str,
    language: String,
    size: u64,
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
    language: String,
    size: u64,
}

#[derive(Debug, Serialize)]
struct TreeResponse {
    tree: String,
    filename: String,
    format: String,
}

#[derive(Debug, Serialize)]
struct SampleResponse {
    path: String,
}

#[derive(Debug, Serialize)]
struct BrowseResponse {
    path: Option<String>,
    cancelled: bool,
}

/// How far the running pack has got, in selected entries.
#[derive(Debug, Serialize)]
struct ProgressResponse {
    running: bool,
    done: usize,
    total: usize,
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

    fn not_found(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: message.into(),
        }
    }

    /// A failure inside the mill (a worker that panicked, say). The details go
    /// to the terminal running `pulp ui`, not to the page.
    fn internal(err: impl std::fmt::Display) -> Self {
        eprintln!("pulp ui: internal error: {err}");
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: "internal error; see the terminal running pulp ui".into(),
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

/// Run blocking work off the async threads.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, ApiError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(ApiError::internal)
}

/// Run blocking work once one of the [`WORK_SLOTS`] is free. The slot stays
/// taken until the work ends, even if the client has gone.
async fn heavy<T: Send + 'static>(
    state: &AppState,
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, ApiError> {
    let slot = Arc::clone(&state.work)
        .acquire_owned()
        .await
        .map_err(ApiError::internal)?;
    blocking(move || {
        let _slot = slot;
        work()
    })
    .await
}

async fn scan(
    State(state): State<AppState>,
    Json(req): Json<ScanRequest>,
) -> Result<Json<ScanResponse>, ApiError> {
    let mill = Arc::clone(&state.mill);
    heavy(&state, move || scan_sync(req, &mill))
        .await?
        .map(Json)
}

/// Holds the folder picker. Frees it when the dialog returns, even if the
/// HTTP request has gone or the picker panicked.
struct PickerSlot(Arc<AtomicBool>);

impl PickerSlot {
    fn take(flag: &Arc<AtomicBool>) -> Option<Self> {
        flag.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .ok()?;
        Some(Self(Arc::clone(flag)))
    }
}

impl Drop for PickerSlot {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

async fn browse(State(state): State<AppState>) -> Result<Json<BrowseResponse>, ApiError> {
    // One dialog at a time: a second Browse (another tab, a double click)
    // would stack dialogs and hold a blocking thread for each.
    let slot = PickerSlot::take(&state.picking).ok_or_else(|| ApiError {
        status: StatusCode::CONFLICT,
        message: "A folder picker is already open. Choose a folder in it or close it.".into(),
    })?;
    let pick = state.pick;
    let chosen = blocking(move || {
        let _slot = slot;
        pick()
    })
    .await?
    .map_err(ApiError::bad)?;
    match chosen {
        Some(path) => Ok(Json(BrowseResponse {
            path: Some(shorten_home(&path, home_dir().as_deref())),
            cancelled: false,
        })),
        None => Ok(Json(BrowseResponse {
            path: None,
            cancelled: true,
        })),
    }
}

/// Write the built-in sample project to a temp folder and return its path.
async fn sample(State(state): State<AppState>) -> Result<Json<SampleResponse>, ApiError> {
    let scratch = Arc::clone(&state.sample);
    let root = blocking(move || scratch.materialize())
        .await?
        .map_err(ApiError::bad)?;
    Ok(Json(SampleResponse {
        path: root.display().to_string(),
    }))
}

async fn pack_dump(
    State(state): State<AppState>,
    Json(req): Json<PackRequest>,
) -> Result<Json<PackResponse>, ApiError> {
    let job = state.mill.try_begin_pack().ok_or_else(ApiError::busy)?;
    // The guard moves into the worker, so the slot frees when the work ends,
    // or when the worker is dropped without running.
    let guard = JobGuard {
        mill: Arc::clone(&state.mill),
        job,
    };
    let mill = Arc::clone(&state.mill);
    blocking(move || pack_sync(req, &mill, &guard.job))
        .await?
        .map(Json)
}

/// Entries the running pack has finished, for a determinate progress rule.
async fn progress(State(state): State<AppState>) -> Response {
    let body = match state.mill.pack_progress() {
        Some((done, total)) => ProgressResponse {
            running: true,
            done,
            total,
        },
        None => ProgressResponse {
            running: false,
            done: 0,
            total: 0,
        },
    };
    ([(header::CACHE_CONTROL, "no-store")], Json(body)).into_response()
}

async fn render_dump(
    State(state): State<AppState>,
    Json(req): Json<PackRequest>,
) -> Result<Json<PackResponse>, ApiError> {
    let mill = Arc::clone(&state.mill);
    heavy(&state, move || render_sync(req, &mill))
        .await?
        .map(Json)
}

async fn cancel_pack(State(state): State<AppState>) -> StatusCode {
    state.mill.request_cancel();
    StatusCode::NO_CONTENT
}

async fn artifact(
    State(state): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
    axum::extract::Query(query): axum::extract::Query<ArtifactQuery>,
) -> Result<Response, ApiError> {
    let mill = Arc::clone(&state.mill);
    heavy(&state, move || artifact_sync(&id, &query, &mill)).await?
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
    Json(req): Json<PackRequest>,
) -> Result<Json<TreeResponse>, ApiError> {
    let mill = Arc::clone(&state.mill);
    heavy(&state, move || tree_sync(req, &mill))
        .await?
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
        exclude: req.exclude,
        default_excludes: !req.no_default_excludes,
        list_only: true,
        ..Options::default()
    };
    let (manifest, warnings) = manifest::scan_manifest_with_warnings(&opts)
        .map_err(|err| ApiError::bad(err.to_string()))?;
    let files: Vec<FileEntry> = manifest
        .entries
        .iter()
        .map(|entry| FileEntry {
            id: entry.id.clone(),
            relative: entry.relative.clone(),
            size: entry.size,
            kind: entry.kind.as_str(),
            language: entry.language.clone(),
            default_on: entry.default_on,
            oversized: entry.oversized,
        })
        .collect();
    let truncated = manifest.truncated;
    let bytes = manifest.bytes;
    let stored = mill.put_manifest(key, manifest);
    Ok(ScanResponse {
        warnings: warnings
            .messages
            .iter()
            .take(SCAN_WARNINGS_SHOWN)
            .map(|message| scan_warning(message, &root))
            .collect(),
        warning_count: warnings.total,
        root: root.display().to_string(),
        file_count: files.len(),
        bytes,
        files,
        truncated,
        manifest_id: stored.id.clone(),
    })
}

/// A walk warning as the mill shows it: paths under the scanned folder are
/// written relative to it, as the file list writes them.
fn scan_warning(message: &str, root: &Path) -> String {
    let prefix = format!("{}{}", root.display(), std::path::MAIN_SEPARATOR);
    message.replace(&prefix, "")
}

fn pack_sync(req: PackRequest, mill: &Mill, job: &PackJob) -> Result<PackResponse, ApiError> {
    if req.selected.is_empty() {
        return Err(ApiError::bad("tick at least one file"));
    }
    let extract_key = extract_key(&req);
    let opts = options_from_pack(req.clone(), false, false)?;
    let scan = load_or_scan_manifest(&req, mill)?;
    if let Some(hit) = mill.find_result(&scan.id, &extract_key) {
        return finish_pack(hit, &opts, true);
    }
    let snapshot = selected_manifest(&scan.manifest, &req.selected);
    job.total.store(snapshot.entries.len(), Ordering::SeqCst);
    let packed = pack::pack_manifest(
        &snapshot,
        &opts,
        Some(&job.cancel),
        Some(&job.done),
        Some(Instant::now()),
    )
    .map_err(|err| ApiError::bad(err.to_string()))?;
    let stored = mill.put_result(StoredResult {
        id: store::new_id(),
        manifest_id: scan.id.clone(),
        extract_key,
        files: packed.files,
        stats: packed.stats,
        roots: opts.roots.clone(),
        source_mode: opts.source_mode,
    });
    finish_pack(stored, &opts, false)
}

fn render_sync(req: PackRequest, mill: &Mill) -> Result<PackResponse, ApiError> {
    if req.result_id.is_empty() {
        return Err(ApiError::bad("missing result_id"));
    }
    let stored = mill
        .get_result(&req.result_id)
        .ok_or_else(|| ApiError::not_found("unknown result"))?;
    let opts = options_from_pack(req, false, false)?;
    finish_pack(stored, &opts, true)
}

fn artifact_sync(id: &str, query: &ArtifactQuery, mill: &Mill) -> Result<Response, ApiError> {
    let stored = mill
        .get_result(id)
        .ok_or_else(|| ApiError::not_found("unknown result"))?;
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
        roots: stored.roots.clone(),
        source_mode: stored.source_mode,
        ..Options::default()
    };
    let packed = packed_from_stored(&stored, &opts);
    let mut dump = Cursor::new(Vec::new());
    crate::render::write_all(&mut dump, &packed, &opts)
        .map_err(|err| ApiError::bad(err.to_string()))?;
    let body = dump.into_inner();
    let disposition = match format {
        OutputFormat::Plain => "attachment; filename=\"pulp.txt\"",
        OutputFormat::Markdown => "attachment; filename=\"pulp.md\"",
        OutputFormat::Xml => "attachment; filename=\"pulp.xml\"",
    };
    Ok((
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/plain; charset=utf-8"),
            ),
            (
                header::CONTENT_DISPOSITION,
                HeaderValue::from_static(disposition),
            ),
        ],
        body,
    )
        .into_response())
}

/// The request's stored manifest when it was scanned with the same discovery
/// settings; otherwise a fresh scan, stored for the requests that follow.
fn load_or_scan_manifest(req: &PackRequest, mill: &Mill) -> Result<Arc<StoredManifest>, ApiError> {
    let want = discovery_key_pack(req);
    if !req.manifest_id.is_empty() {
        if let Some(stored) = mill.get_manifest(&req.manifest_id) {
            if stored.discovery_key == want {
                return Ok(stored);
            }
            return Err(ApiError::bad("manifest settings changed; rescan"));
        }
    }
    let opts = options_from_pack(req.clone(), true, false)?;
    let manifest = manifest::scan_manifest(&opts).map_err(|err| ApiError::bad(err.to_string()))?;
    Ok(mill.put_manifest(want, manifest))
}

/// A copy of `manifest` with only the entries the request ticked, matched by
/// id first (see [`SelectionFilter`]). Copies those entries alone, not the
/// whole scan.
fn selected_manifest(manifest: &ScanManifest, selected: &[String]) -> ScanManifest {
    let want = SelectionFilter::only(
        selected,
        manifest.entries.iter().map(|entry| entry.id.as_str()),
    );
    ScanManifest {
        root: manifest.root.clone(),
        entries: manifest
            .entries
            .iter()
            .filter(|entry| want.contains(&entry.id, &entry.relative))
            .cloned()
            .collect(),
        bytes: manifest.bytes,
        truncated: manifest.truncated,
    }
}

fn finish_pack(
    stored: Arc<StoredResult>,
    opts: &Options,
    cache_hit: bool,
) -> Result<PackResponse, ApiError> {
    let render_start = Instant::now();
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
            kind: file.kind.as_str(),
            language: crate::language_name(std::path::Path::new(&file.relative), file.kind),
            size: file.size,
            message: file.status.message(file.size),
        })
        .collect();
    let extract_ms = stored.stats.elapsed.as_millis();
    let render_ms = render_start.elapsed().as_millis();
    Ok(PackResponse {
        dump,
        dump_bytes,
        filename: format!("pulp.{ext}"),
        format: ext.to_string(),
        files_extracted: packed.stats.files_extracted,
        files_skipped: packed.stats.files_skipped,
        tokens_est: packed.stats.tokens_est,
        chars_emitted: packed.stats.chars_emitted,
        elapsed_ms: extract_ms.saturating_add(render_ms),
        extract_ms,
        render_ms,
        cache_hit,
        truncated: packed.stats.truncated,
        cancelled: packed.stats.cancelled,
        preview_truncated,
        manifest_id: stored.manifest_id.clone(),
        result_id: stored.id.clone(),
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
        crate::tree::render_tree(&pack::tree_label(&stored.roots), &paths)
    };
    // Count what this drawing holds, the map included or not, the way a fresh
    // pack with these settings would.
    let mut stats = stored.stats.clone();
    let extracted = stored
        .files
        .iter()
        .filter(|f| f.status == pack::FileStatus::Extracted)
        .map(|f| f.text.as_str());
    (stats.chars_emitted, stats.tokens_est) =
        crate::tokens::summarize_chunks(std::iter::once(tree.as_str()).chain(extracted));
    pack::Packed {
        files: stored.files.clone(),
        tree,
        stats,
    }
}

/// Everything that decides which files a scan finds. A stored manifest is
/// reused only by a request with the same key.
fn discovery_key(req: &ScanRequest) -> String {
    format!(
        "{}|{}|{}|{}|{}|{:?}",
        req.path, req.hidden, req.gitignore, req.archives, req.no_default_excludes, req.exclude
    )
}

fn discovery_key_pack(req: &PackRequest) -> String {
    format!(
        "{}|{}|{}|{}|{}|{:?}",
        req.path, req.hidden, req.gitignore, req.archives, req.no_default_excludes, req.exclude
    )
}

fn extract_key(req: &PackRequest) -> String {
    let mut selected = req.selected.clone();
    selected.sort();
    serde_json::json!({
        "selected": selected,
        "source": req.source,
        "notebook_outputs": req.notebook_outputs,
        "archives": req.archives,
        "binaries": req.binaries,
        "max_file_size": req.max_file_size,
    })
    .to_string()
}

async fn preview(
    State(state): State<AppState>,
    Json(req): Json<PackRequest>,
) -> Result<Json<PreviewResponse>, ApiError> {
    let slot = state.mill.try_begin_preview().ok_or_else(ApiError::busy)?;
    let mill = Arc::clone(&state.mill);
    blocking(move || {
        let _slot = slot;
        preview_sync(req, &mill)
    })
    .await?
    .map(Json)
}

/// Extract the one selected file. With a usable manifest only that entry is
/// read; otherwise the folder is walked to find it.
fn preview_sync(req: PackRequest, mill: &Mill) -> Result<PreviewResponse, ApiError> {
    let [want] = req.selected.as_slice() else {
        return Err(ApiError::bad("preview one file"));
    };
    let opts = options_from_pack(req.clone(), false, false)?;
    let entry = match stored_entry(&req, want, mill)? {
        Some(entry) => entry,
        None => walked_entry(&opts)?,
    };
    let mut files = pack::pack_manifest_entry(&entry, &opts);
    // Same shape as the browser mill's preview: the extracted text, or empty
    // text and the reason; an expanded archive shows every member.
    let (full, status, message) = match files.as_slice() {
        [] => (
            String::new(),
            pack::FileStatus::Extracted.as_str(),
            "Nothing in this file goes into the dump.".to_string(),
        ),
        [file] if file.id == entry.id => {
            let file = files.swap_remove(0);
            let status = file.status.as_str();
            let message = file.status.message(file.size);
            let text = if file.status == pack::FileStatus::Extracted {
                file.text
            } else {
                String::new()
            };
            (text, status, message)
        }
        members => {
            let (status, message) = if members
                .iter()
                .any(|f| f.status == pack::FileStatus::Extracted)
            {
                (pack::FileStatus::Extracted.as_str(), String::new())
            } else {
                (
                    members[0].status.as_str(),
                    members[0].status.message(members[0].size),
                )
            };
            (plain_members(files, &opts)?, status, message)
        }
    };
    let truncated = full.len() > FILE_PREVIEW_BYTES;
    let text = if truncated {
        let mut end = FILE_PREVIEW_BYTES;
        while end > 0 && !full.is_char_boundary(end) {
            end -= 1;
        }
        full[..end].to_string()
    } else {
        full
    };
    Ok(PreviewResponse {
        id: entry.id.clone(),
        relative: entry.relative.clone(),
        status,
        message,
        kind: entry.kind.as_str(),
        language: crate::language_name(std::path::Path::new(&entry.relative), entry.kind),
        size: entry.size,
        truncated,
        text,
    })
}

/// A file preview's cap, the same as the browser mill's.
const FILE_PREVIEW_BYTES: usize = 32 * 1024;

/// An expanded archive's members as a plain-text dump with no directory map.
fn plain_members(files: Vec<pack::PackedFile>, opts: &Options) -> Result<String, ApiError> {
    let packed = pack::Packed {
        files,
        tree: String::new(),
        stats: pack::Stats::default(),
    };
    let plain = Options {
        format: OutputFormat::Plain,
        tree: TreeMode::None,
        source_mode: opts.source_mode,
        ..Options::default()
    };
    let mut out = Vec::new();
    crate::render::write_all(&mut out, &packed, &plain)
        .map_err(|err| ApiError::bad(err.to_string()))?;
    Ok(String::from_utf8_lossy(&out).into_owned())
}

/// The selected file's entry in the request's stored manifest, matched by id or,
/// when no entry has that id, by relative path, so a request can never name a
/// path of its own. `Ok(None)` when the manifest id is unknown or was scanned
/// with other discovery settings.
fn stored_entry(
    req: &PackRequest,
    want: &str,
    mill: &Mill,
) -> Result<Option<ManifestEntry>, ApiError> {
    let Some(stored) = mill.get_manifest(&req.manifest_id) else {
        return Ok(None);
    };
    if stored.discovery_key != discovery_key_pack(req) {
        return Ok(None);
    }
    let entries = &stored.manifest.entries;
    entries
        .iter()
        .find(|entry| entry.id == want)
        .or_else(|| entries.iter().find(|entry| entry.relative == want))
        .cloned()
        .map(Some)
        .ok_or_else(|| ApiError::bad("file is not in this scan; rescan"))
}

/// Walk the folder for the selected file when there is no usable manifest.
fn walked_entry(opts: &Options) -> Result<ManifestEntry, ApiError> {
    manifest::scan_manifest(opts)
        .map_err(|err| ApiError::bad(err.to_string()))?
        .entries
        .into_iter()
        .next()
        .ok_or_else(|| ApiError::bad("file not found"))
}

fn tree_sync(req: PackRequest, mill: &Mill) -> Result<TreeResponse, ApiError> {
    if req.selected.is_empty() {
        return Err(ApiError::bad("tick at least one file"));
    }
    let opts = options_from_pack(req.clone(), true, true)?;
    let scan = load_or_scan_manifest(&req, mill)?;
    let paths: Vec<String> = selected_manifest(&scan.manifest, &req.selected)
        .entries
        .into_iter()
        .map(|entry| entry.relative)
        .collect();
    let tree =
        crate::render::format_directory_map(&pack::tree_label(&opts.roots), &paths, opts.format)
            .map_err(|err| ApiError::bad(err.to_string()))?;
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
        exclude: req.exclude,
        default_excludes: !req.no_default_excludes,
        max_file_size,
        list_only,
        ..Options::default()
    })
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

/// `path` with `home` written as `~`, the form [`expand_tilde`] reads back.
/// A path outside `home`, or any path when `home` is unknown or the filesystem
/// root, comes back unchanged.
fn shorten_home(path: &Path, home: Option<&Path>) -> String {
    let rest = home
        .filter(|home| home.parent().is_some())
        .and_then(|home| path.strip_prefix(home).ok());
    match rest {
        Some(rest) if rest.as_os_str().is_empty() => "~".to_string(),
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
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
        post_to(&router(), uri, body).await
    }

    /// POST to one app, so later requests see the manifests earlier ones stored.
    async fn post_to(
        app: &Router,
        uri: &str,
        body: serde_json::Value,
    ) -> (StatusCode, serde_json::Value) {
        let response = app
            .clone()
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

    fn router_with_mill(mill: Arc<Mill>) -> Router {
        router_with(AppState {
            mill,
            ..AppState::new(TEST_TOKEN, TEST_ORIGIN)
        })
    }

    /// GET as a browser on the mill's own address sends it, with no token.
    async fn get(uri: &str) -> Response {
        router()
            .oneshot(
                Request::builder()
                    .uri(uri)
                    .header(header::HOST, "127.0.0.1:8747")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    /// The mill page, opened from the link `pulp ui` prints.
    async fn get_page() -> Response {
        get(&format!("/?token={TEST_TOKEN}")).await
    }

    async fn body_text(response: Response) -> String {
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    /// GET with the session token, as the mill shell sends it.
    async fn get_authorized(app: &Router, uri: &str) -> Response {
        app.clone()
            .oneshot(
                Request::builder()
                    .uri(uri)
                    .header(header::HOST, "127.0.0.1:8747")
                    .header("x-pulp-token", TEST_TOKEN)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn test_font_with_plex_sans_returns_woff2() {
        let response = get("/fonts/plex-sans.woff2").await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE).unwrap(),
            "font/woff2"
        );
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        assert!(bytes.len() > 1000);
    }

    #[tokio::test]
    async fn test_font_with_fraunces_returns_not_found() {
        let response = get("/fonts/fraunces.woff2").await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_assets_return_css_and_module_script() {
        for (uri, content_type, marker) in [
            ("/pulp.css", "text/css; charset=utf-8", "--acc:"),
            ("/mill.css", "text/css; charset=utf-8", ".mill-grid"),
            (
                "/mill.js",
                "text/javascript; charset=utf-8",
                "export function mountMill",
            ),
        ] {
            let response = get(uri).await;
            assert_eq!(response.status(), StatusCode::OK, "{uri}");
            assert_eq!(
                response.headers().get(header::CONTENT_TYPE).unwrap(),
                content_type,
                "{uri}"
            );
            assert_eq!(
                response.headers().get(header::CACHE_CONTROL).unwrap(),
                "no-cache",
                "{uri}"
            );
            let text = body_text(response).await;
            assert!(text.contains(marker), "{uri} lacks {marker}");
        }
    }

    #[tokio::test]
    async fn test_mill_js_keeps_the_github_report_flow() {
        let js = body_text(get("/mill.js").await).await;
        assert!(js.contains("https://github.com/BeeGass/pulp/issues/new"));
        assert!(js.contains("Open GitHub issue"));
        assert!(js.contains("Files stayed on this machine."));
    }

    #[tokio::test]
    async fn test_health_returns_ok() {
        let response = router()
            .oneshot(
                Request::builder()
                    .uri("/api/health")
                    .header(header::HOST, "127.0.0.1:8747")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_index_returns_mill_shell_with_token_and_version() {
        let response = get_page().await;
        assert_eq!(response.status(), StatusCode::OK);
        let html = body_text(response).await;
        assert!(html.contains("<title>pulp mill</title>"));
        assert!(html.contains("href=\"/pulp.css\""));
        assert!(html.contains("href=\"/mill.css\""));
        assert!(html.contains("from '/mill.js'"));
        assert!(html.contains(TEST_TOKEN));
        assert!(!html.contains("__PULP_TOKEN__"));
        assert!(html.contains(env!("CARGO_PKG_VERSION")));
        assert!(!html.contains("__PULP_VERSION__"));
        assert!(html.contains("127.0.0.1 only"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_scan_with_unreadable_folder_returns_warning_and_other_files() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn a() {}\n").unwrap();
        let locked = dir.path().join("locked");
        std::fs::create_dir(&locked).unwrap();
        std::fs::write(locked.join("b.rs"), "fn b() {}\n").unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let privileged = std::fs::read_dir(&locked).is_ok();
        let (status, json) = post_json(
            "/api/scan",
            serde_json::json!({ "path": dir.path().display().to_string() }),
        )
        .await;
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        if privileged {
            // Running as root: nothing is unreadable, so there is nothing to test.
            return;
        }
        assert_eq!(status, StatusCode::OK, "{json}");
        assert_eq!(json["file_count"], 1, "{json}");
        assert_eq!(json["warning_count"], 1, "{json}");
        let warning = json["warnings"][0].as_str().unwrap();
        assert!(warning.starts_with("locked: "), "{warning}");
    }

    #[cfg(unix)]
    #[test]
    fn test_scan_warning_with_path_under_root_returns_relative_text() {
        let root = Path::new("/home/ada/tides");
        assert_eq!(
            scan_warning(
                "/home/ada/tides/locked: Permission denied (os error 13)",
                root
            ),
            "locked: Permission denied (os error 13)"
        );
        assert_eq!(
            scan_warning("/elsewhere/x: Permission denied (os error 13)", root),
            "/elsewhere/x: Permission denied (os error 13)"
        );
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
        assert!(mill.try_begin_pack().is_some());
        let app = router_with(AppState {
            mill,
            ..AppState::new(TEST_TOKEN, TEST_ORIGIN)
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
    async fn test_render_with_map_toggled_counts_like_a_fresh_pack() {
        let app = router();
        let path = testdata().display().to_string();
        let body = |extra: serde_json::Value| {
            let mut req = serde_json::json!({
                "path": path,
                "format": "txt",
                "gitignore": false,
                "selected": ["hello.rs"]
            });
            req.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            req
        };
        let (status, with_map) = post_to(&app, "/api/pack", body(serde_json::json!({}))).await;
        assert_eq!(status, StatusCode::OK, "{with_map}");
        let result_id = with_map["result_id"].clone();
        let (status, redrawn) = post_to(
            &app,
            "/api/render",
            body(serde_json::json!({ "no_tree": true, "result_id": result_id })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{redrawn}");
        // `binaries` changes the extraction key, so this is a real pack to compare
        // with, not another redraw of the stored result.
        let (status, fresh) = post_to(
            &app,
            "/api/pack",
            body(serde_json::json!({ "no_tree": true, "binaries": true })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{fresh}");
        assert!(
            redrawn["chars_emitted"].as_u64() < with_map["chars_emitted"].as_u64(),
            "{redrawn} vs {with_map}"
        );
        assert_eq!(redrawn["chars_emitted"], fresh["chars_emitted"]);
        assert_eq!(redrawn["tokens_est"], fresh["tokens_est"]);
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
    async fn test_preview_with_manifest_id_returns_source() {
        let app = router();
        let path = testdata().display().to_string();
        let (status, scan) = post_to(
            &app,
            "/api/scan",
            serde_json::json!({ "path": path, "gitignore": false }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{scan}");
        let manifest_id = scan["manifest_id"].as_str().unwrap();
        // Two previews in a row: the first must hand its slot back.
        for (file, marker, language) in [
            ("hello.rs", "fn hello", "rust"),
            ("Hello.lean", "def hello", "lean"),
        ] {
            let (status, json) = post_to(
                &app,
                "/api/preview",
                serde_json::json!({
                    "path": path,
                    "gitignore": false,
                    "manifest_id": manifest_id,
                    "selected": [file]
                }),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{json}");
            assert_eq!(json["relative"].as_str(), Some(file));
            assert_eq!(json["status"].as_str(), Some("extracted"));
            assert_eq!(json["language"].as_str(), Some(language));
            assert!(json["text"].as_str().unwrap().contains(marker), "{json}");
        }
    }

    #[tokio::test]
    async fn test_preview_with_expanded_archive_shows_every_member() {
        use std::io::Write as _;
        let dir = tempfile::tempdir().unwrap();
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut zw = zip::ZipWriter::new(&mut buf);
            let opt = zip::write::SimpleFileOptions::default();
            zw.start_file("first.txt", opt).unwrap();
            zw.write_all(b"alpha member\n").unwrap();
            zw.start_file("second.txt", opt).unwrap();
            zw.write_all(b"beta member\n").unwrap();
            zw.finish().unwrap();
        }
        std::fs::write(dir.path().join("bundle.zip"), buf.into_inner()).unwrap();
        let app = router();
        let path = dir.path().display().to_string();
        let (status, scan) = post_to(
            &app,
            "/api/scan",
            serde_json::json!({ "path": path, "archives": true }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{scan}");
        let (status, json) = post_to(
            &app,
            "/api/preview",
            serde_json::json!({
                "path": path,
                "archives": true,
                "manifest_id": scan["manifest_id"],
                "selected": ["bundle.zip"]
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{json}");
        let text = json["text"].as_str().unwrap();
        assert!(
            text.contains("alpha member") && text.contains("beta member"),
            "{json}"
        );
        assert_eq!(json["status"], "extracted");
        assert_eq!(json["relative"], "bundle.zip");
    }

    #[tokio::test]
    async fn test_preview_with_damaged_file_returns_reason_and_no_text() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("paper.pdf"), b"%PDF-1.4\nnot a real pdf\n").unwrap();
        let app = router();
        let path = dir.path().display().to_string();
        let (status, scan) = post_to(&app, "/api/scan", serde_json::json!({ "path": path })).await;
        assert_eq!(status, StatusCode::OK, "{scan}");
        let (status, json) = post_to(
            &app,
            "/api/preview",
            serde_json::json!({
                "path": path,
                "manifest_id": scan["manifest_id"],
                "selected": ["paper.pdf"]
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{json}");
        assert_eq!(json["status"], "unreadable");
        assert_eq!(json["text"], "");
        assert!(
            json["message"]
                .as_str()
                .unwrap()
                .contains("Could not parse"),
            "{json}"
        );
    }

    #[tokio::test]
    async fn test_preview_with_large_file_returns_capped_text() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("wide.txt"), "\u{20ac}".repeat(12_000)).unwrap();
        let app = router();
        let path = dir.path().display().to_string();
        let (status, scan) = post_to(&app, "/api/scan", serde_json::json!({ "path": path })).await;
        assert_eq!(status, StatusCode::OK, "{scan}");
        let (status, json) = post_to(
            &app,
            "/api/preview",
            serde_json::json!({
                "path": path,
                "manifest_id": scan["manifest_id"],
                "selected": ["wide.txt"]
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{json}");
        let mut keys: Vec<&str> = json
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "id",
                "kind",
                "language",
                "message",
                "relative",
                "size",
                "status",
                "text",
                "truncated"
            ]
        );
        // 32 KiB ends inside a three-byte character, so the cut backs up two bytes.
        assert_eq!(json["text"].as_str().unwrap().len(), 32 * 1024 - 2);
        assert_eq!(json["truncated"], true);
        assert_eq!(json["size"], 36_000);
    }

    #[tokio::test]
    async fn test_preview_with_manifest_id_after_folder_change_returns_scanned_files_only() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("kept.rs"), "fn kept() {}\n").unwrap();
        let app = router();
        let path = dir.path().display().to_string();
        let (status, scan) = post_to(&app, "/api/scan", serde_json::json!({ "path": path })).await;
        assert_eq!(status, StatusCode::OK, "{scan}");
        let manifest_id = scan["manifest_id"].as_str().unwrap();
        // A fresh walk would now skip kept.rs and find new.rs.
        std::fs::write(dir.path().join(".ignore"), "kept.rs\n").unwrap();
        std::fs::write(dir.path().join("new.rs"), "fn new() {}\n").unwrap();
        let body = |file: &str| {
            serde_json::json!({
                "path": path,
                "manifest_id": manifest_id,
                "selected": [file]
            })
        };

        let (status, json) = post_to(&app, "/api/preview", body("kept.rs")).await;
        assert_eq!(status, StatusCode::OK, "{json}");
        assert!(json["text"].as_str().unwrap().contains("fn kept"), "{json}");

        let (status, json) = post_to(&app, "/api/preview", body("new.rs")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{json}");
        assert_eq!(
            json["error"].as_str(),
            Some("file is not in this scan; rescan")
        );
    }

    #[tokio::test]
    async fn test_preview_with_id_outside_manifest_returns_bad_request() {
        let app = router();
        let path = testdata().display().to_string();
        let (status, scan) = post_to(
            &app,
            "/api/scan",
            serde_json::json!({ "path": path, "gitignore": false }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{scan}");
        let outside = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("Cargo.toml")
            .display()
            .to_string();
        for file in ["missing.rs", "../Cargo.toml", outside.as_str()] {
            let (status, json) = post_to(
                &app,
                "/api/preview",
                serde_json::json!({
                    "path": path,
                    "gitignore": false,
                    "manifest_id": scan["manifest_id"],
                    "selected": [file]
                }),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{file}: {json}");
            assert_eq!(
                json["error"].as_str(),
                Some("file is not in this scan; rescan"),
                "{file}"
            );
            // Without a manifest the walk cannot reach outside the folder either.
            let (status, json) = post_to(
                &app,
                "/api/preview",
                serde_json::json!({ "path": path, "gitignore": false, "selected": [file] }),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{file}: {json}");
        }
    }

    #[tokio::test]
    async fn test_preview_with_stale_manifest_id_returns_walked_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn a() {}\n").unwrap();
        let app = router();
        let path = dir.path().display().to_string();
        let (status, scan) = post_to(&app, "/api/scan", serde_json::json!({ "path": path })).await;
        assert_eq!(status, StatusCode::OK, "{scan}");
        // Only a fresh walk can find a file added after the scan.
        std::fs::write(dir.path().join("b.rs"), "fn b() {}\n").unwrap();
        for body in [
            serde_json::json!({
                "path": path,
                "manifest_id": "0123456789abcdef",
                "selected": ["b.rs"]
            }),
            serde_json::json!({
                "path": path,
                "manifest_id": scan["manifest_id"],
                "hidden": true,
                "selected": ["b.rs"]
            }),
        ] {
            let (status, json) = post_to(&app, "/api/preview", body).await;
            assert_eq!(status, StatusCode::OK, "{json}");
            assert!(json["text"].as_str().unwrap().contains("fn b"), "{json}");
        }
    }

    #[tokio::test]
    async fn test_preview_with_pack_running_returns_source() {
        let mill = Arc::new(Mill::new());
        let _pack = mill.try_begin_pack().expect("pack slot");
        let app = router_with_mill(mill);
        let path = testdata().display().to_string();
        let (status, json) = post_to(
            &app,
            "/api/preview",
            serde_json::json!({ "path": path, "gitignore": false, "selected": ["hello.rs"] }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{json}");
        assert!(
            json["text"].as_str().unwrap().contains("fn hello"),
            "{json}"
        );
    }

    #[tokio::test]
    async fn test_pack_with_preview_running_returns_dump() {
        let mill = Arc::new(Mill::new());
        let _preview = mill.try_begin_preview().expect("preview slot");
        let app = router_with_mill(mill);
        let path = testdata().display().to_string();
        let (status, json) = post_to(
            &app,
            "/api/pack",
            serde_json::json!({
                "path": path,
                "format": "txt",
                "gitignore": false,
                "selected": ["hello.rs"]
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{json}");
        assert!(
            json["dump"].as_str().unwrap().contains("fn hello"),
            "{json}"
        );
    }

    #[tokio::test]
    async fn test_preview_busy_returns_too_many_requests() {
        let mill = Arc::new(Mill::new());
        let _preview = mill.try_begin_preview().expect("preview slot");
        let app = router_with_mill(mill);
        let path = testdata().display().to_string();
        let (status, json) = post_to(
            &app,
            "/api/preview",
            serde_json::json!({ "path": path, "gitignore": false, "selected": ["hello.rs"] }),
        )
        .await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{json}");
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
            ..AppState::new(TEST_TOKEN, TEST_ORIGIN)
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
            ..AppState::new(TEST_TOKEN, TEST_ORIGIN)
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

    #[cfg(unix)]
    fn stub_pick_in_home() -> Result<Option<PathBuf>, String> {
        Ok(home_dir().map(|home| home.join("Projects").join("pulp")))
    }

    // These two need a home folder and `/` separators.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_browse_with_folder_in_home_returns_tilde_path() {
        if home_dir().is_none() {
            return;
        }
        let app = router_with(AppState {
            pick: stub_pick_in_home,
            ..AppState::new(TEST_TOKEN, TEST_ORIGIN)
        });
        let (status, json) = post_to(&app, "/api/browse", serde_json::json!({})).await;
        assert_eq!(status, StatusCode::OK, "{json}");
        assert_eq!(json["path"].as_str(), Some("~/Projects/pulp"), "{json}");
    }

    #[test]
    fn test_shorten_home_with_path_inside_home_returns_tilde_path() {
        let home = Path::new("/Users/bee");
        assert_eq!(
            shorten_home(Path::new("/Users/bee/Projects/pulp"), Some(home)),
            "~/Projects/pulp"
        );
        // The macOS picker ends a folder with a slash.
        assert_eq!(
            shorten_home(Path::new("/Users/bee/Projects/pulp/"), Some(home)),
            "~/Projects/pulp"
        );
    }

    #[test]
    fn test_shorten_home_with_home_itself_returns_tilde() {
        let home = Path::new("/Users/bee");
        assert_eq!(shorten_home(Path::new("/Users/bee"), Some(home)), "~");
        assert_eq!(shorten_home(Path::new("/Users/bee/"), Some(home)), "~");
    }

    #[test]
    fn test_shorten_home_with_path_outside_home_returns_path_unchanged() {
        let home = Path::new("/Users/bee");
        assert_eq!(
            shorten_home(Path::new("/Volumes/data/pulp"), Some(home)),
            "/Volumes/data/pulp"
        );
        // A shared name prefix is not a parent folder.
        assert_eq!(
            shorten_home(Path::new("/Users/beegass/pulp"), Some(home)),
            "/Users/beegass/pulp"
        );
        assert_eq!(
            shorten_home(Path::new("/srv/pulp"), Some(Path::new("/"))),
            "/srv/pulp"
        );
    }

    #[test]
    fn test_shorten_home_with_home_unset_returns_path_unchanged() {
        assert_eq!(
            shorten_home(Path::new("/Users/bee/Projects/pulp"), None),
            "/Users/bee/Projects/pulp"
        );
    }

    #[test]
    #[cfg(unix)]
    fn test_expand_tilde_with_shortened_path_returns_original_path() {
        let Some(home) = home_dir() else {
            return;
        };
        let path = home.join("Projects").join("pulp");
        let short = shorten_home(&path, Some(&home));
        assert_eq!(short, "~/Projects/pulp");
        assert_eq!(expand_tilde(&short), path);
        assert_eq!(expand_tilde(&shorten_home(&home, Some(&home))), home);
    }

    /// The mill sends back whatever path browse showed. With the checkout under
    /// the home folder, that is the `~` form.
    #[tokio::test]
    async fn test_mill_with_home_relative_path_returns_dump_preview_and_tree() {
        let app = router();
        let path = shorten_home(&testdata(), home_dir().as_deref());
        let (status, scan) = post_to(
            &app,
            "/api/scan",
            serde_json::json!({ "path": path, "gitignore": false }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{path}: {scan}");
        let body = |extra: serde_json::Value| {
            let mut body = serde_json::json!({
                "path": path,
                "gitignore": false,
                "manifest_id": scan["manifest_id"],
                "selected": ["hello.rs"]
            });
            body.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            body
        };

        let (status, packed) = post_to(&app, "/api/pack", body(serde_json::json!({}))).await;
        assert_eq!(status, StatusCode::OK, "{packed}");
        assert!(packed["dump"].as_str().unwrap().contains("fn hello"));

        let (status, preview) = post_to(&app, "/api/preview", body(serde_json::json!({}))).await;
        assert_eq!(status, StatusCode::OK, "{preview}");
        assert!(preview["text"].as_str().unwrap().contains("fn hello"));

        let (status, tree) = post_to(&app, "/api/tree", body(serde_json::json!({}))).await;
        assert_eq!(status, StatusCode::OK, "{tree}");
        assert!(
            tree["tree"].as_str().unwrap().contains("testdata/"),
            "{tree}"
        );

        let result_id = packed["result_id"].as_str().unwrap();
        let (status, redrawn) = post_to(
            &app,
            "/api/render",
            body(serde_json::json!({ "format": "md", "result_id": result_id })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{redrawn}");
        assert!(redrawn["dump"].as_str().unwrap().contains("## hello.rs"));

        let response = get_authorized(&app, &format!("/api/artifact/{result_id}")).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(body_text(response).await.contains("fn hello"));
    }

    #[tokio::test]
    async fn test_progress_with_missing_token_returns_unauthorized() {
        let response = get("/api/progress").await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn test_progress_with_idle_mill_returns_not_running() {
        let response = get_authorized(&router(), "/api/progress").await;
        assert_eq!(response.status(), StatusCode::OK);
        let json: serde_json::Value = serde_json::from_str(&body_text(response).await).unwrap();
        assert_eq!(
            json,
            serde_json::json!({ "running": false, "done": 0, "total": 0 })
        );
    }

    #[tokio::test]
    async fn test_progress_with_running_job_returns_done_and_total() {
        let mill = Arc::new(Mill::new());
        let job = mill.try_begin_pack().expect("pack slot");
        job.total.store(41, Ordering::SeqCst);
        job.done.store(23, Ordering::SeqCst);
        let response = get_authorized(&router_with_mill(mill), "/api/progress").await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(header::CACHE_CONTROL).unwrap(),
            "no-store"
        );
        let json: serde_json::Value = serde_json::from_str(&body_text(response).await).unwrap();
        assert_eq!(
            json,
            serde_json::json!({ "running": true, "done": 23, "total": 41 })
        );
    }

    #[test]
    fn test_pack_sync_with_two_files_returns_done_and_total_of_two() {
        let mill = Mill::new();
        let job = mill.try_begin_pack().expect("pack slot");
        let req: PackRequest = serde_json::from_value(serde_json::json!({
            "path": testdata().display().to_string(),
            "gitignore": false,
            "selected": ["hello.rs", "Hello.lean"]
        }))
        .unwrap();
        let packed = pack_sync(req, &mill, &job).unwrap_or_else(|err| panic!("{}", err.message));
        assert_eq!(packed.files_extracted, 2);
        assert_eq!(job.total.load(Ordering::SeqCst), 2);
        assert_eq!(job.done.load(Ordering::SeqCst), 2);
        mill.end_pack(&job);
    }

    #[tokio::test]
    async fn test_pack_with_corrupt_pdf_returns_unreadable_outcome_and_note() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("paper.pdf"), b"%PDF-1.4 garbage").unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn a() {}\n").unwrap();
        let (status, json) = post_json(
            "/api/pack",
            serde_json::json!({
                "path": dir.path().display().to_string(),
                "format": "txt",
                "selected": ["paper.pdf", "a.rs"]
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{json}");
        let pdf = json["outcomes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|o| o["relative"] == "paper.pdf")
            .unwrap_or_else(|| panic!("no paper.pdf outcome in {json}"));
        assert_eq!(pdf["status"], "unreadable", "{pdf}");
        let message = pdf["message"].as_str().unwrap();
        let (_, reason) = message
            .rsplit_once("too large to unpack. ")
            .unwrap_or_else(|| panic!("no parser message in {message}"));
        assert!(!reason.is_empty(), "{message}");
        let note = format!("[pdf unreadable: {reason}]");
        assert!(json["dump"].as_str().unwrap().contains(&note), "{json}");
        assert_eq!(json["files_extracted"], 1);
        assert_eq!(json["files_skipped"], 1);
    }

    #[tokio::test]
    async fn test_index_wires_every_mill_endpoint() {
        let html = body_text(get_page().await).await;
        assert!(html.contains("x-pulp-token"));
        for endpoint in [
            "/api/browse",
            "/api/sample",
            "/api/scan",
            "/api/pack",
            "/api/render",
            "/api/preview",
            "/api/tree",
            "/api/cancel",
            "/api/progress",
            "/api/artifact/",
        ] {
            assert!(html.contains(endpoint), "shell does not call {endpoint}");
        }
        assert!(html.contains("progress: true"), "shell hides pack progress");
    }

    #[tokio::test]
    async fn test_sample_writes_project_that_scans() {
        let (status, json) = post_json("/api/sample", serde_json::json!({})).await;
        assert_eq!(status, StatusCode::OK, "{json}");
        let path = json["path"].as_str().unwrap().to_string();
        assert!(path.ends_with("tides"), "{path}");
        let root = PathBuf::from(&path);
        assert!(root.join("README.md").is_file());
        assert!(root.join("docs/field-notes.pdf").is_file());

        let (status, scan) = post_json(
            "/api/scan",
            serde_json::json!({ "path": path, "gitignore": false }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{scan}");
        let files = scan["files"].as_array().unwrap();
        let find = |rel: &str| {
            files
                .iter()
                .find(|f| f["relative"] == rel)
                .unwrap_or_else(|| panic!("{rel} missing from {scan}"))
        };
        assert_eq!(find("src/lib.rs")["default_on"], true);
        assert_eq!(find("Cargo.lock")["default_on"], false);
        assert_eq!(find("data/archive.zip")["default_on"], false);
        let _ = std::fs::remove_dir_all(root.parent().unwrap());
    }

    #[tokio::test]
    async fn test_sample_with_missing_token_returns_unauthorized() {
        let response = router()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/sample")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::HOST, "127.0.0.1:8747")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
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
                    .header(header::HOST, "127.0.0.1:8747")
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
                    .header(header::HOST, "127.0.0.1:8747")
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
                    .header(header::HOST, "127.0.0.1:8747")
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

    /// Send one request to `app` as given.
    async fn send(app: &Router, request: Request<Body>) -> Response {
        app.clone().oneshot(request).await.unwrap()
    }

    fn header_of<'a>(response: &'a Response, name: &str) -> &'a str {
        response
            .headers()
            .get(name)
            .unwrap_or_else(|| panic!("no {name} header"))
            .to_str()
            .unwrap()
    }

    #[tokio::test]
    async fn test_index_with_foreign_host_returns_forbidden_without_token() {
        // A page on a domain rebound to 127.0.0.1 sends its own name as Host.
        for host in [
            "evil.example:8747",
            "127.0.0.1.evil.example:8747",
            "127.0.0.1:9999",
        ] {
            let response = send(
                &router(),
                Request::builder()
                    .uri(format!("/?token={TEST_TOKEN}"))
                    .header(header::HOST, host)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;
            assert_eq!(response.status(), StatusCode::FORBIDDEN, "{host}");
            assert_eq!(header_of(&response, "x-frame-options"), "SAMEORIGIN");
            let body = body_text(response).await;
            assert!(!body.contains(TEST_TOKEN), "{host} got the token");
        }
    }

    #[tokio::test]
    async fn test_guard_with_missing_or_mismatched_host_returns_forbidden() {
        let app = router();
        let no_host = send(
            &app,
            Request::builder()
                .uri("/api/progress")
                .header("x-pulp-token", TEST_TOKEN)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(no_host.status(), StatusCode::FORBIDDEN);
        // An absolute request line names the host the request is really for.
        let other_authority = send(
            &app,
            Request::builder()
                .uri("http://evil.example:8747/api/progress")
                .header(header::HOST, "127.0.0.1:8747")
                .header("x-pulp-token", TEST_TOKEN)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(other_authority.status(), StatusCode::FORBIDDEN);
        let undecodable = send(
            &app,
            Request::builder()
                .uri("/api/progress")
                .header(
                    header::HOST,
                    HeaderValue::from_bytes(b"127.0.0.1:8747\xff").unwrap(),
                )
                .header("x-pulp-token", TEST_TOKEN)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(undecodable.status(), StatusCode::FORBIDDEN);
        let unknown_path = send(
            &app,
            Request::builder()
                .uri("/nope")
                .header(header::HOST, "evil.example")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(unknown_path.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn test_index_with_localhost_host_redirects_to_loopback_address() {
        let app = router();
        let page = send(
            &app,
            Request::builder()
                .uri(format!("/?token={TEST_TOKEN}"))
                .header(header::HOST, "localhost:8747")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(page.status(), StatusCode::TEMPORARY_REDIRECT);
        assert_eq!(
            header_of(&page, "location"),
            format!("http://127.0.0.1:8747/?token={TEST_TOKEN}")
        );
        // Only the page moves; the API answers on its own address only.
        let api = send(
            &app,
            Request::builder()
                .uri("/api/progress")
                .header(header::HOST, "localhost:8747")
                .header("x-pulp-token", TEST_TOKEN)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(api.status(), StatusCode::FORBIDDEN);
        let other_port = send(
            &app,
            Request::builder()
                .uri("/")
                .header(header::HOST, "localhost:9999")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(other_port.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn test_index_without_token_returns_locked_page() {
        for uri in ["/", "/?token=", "/?token=wrong-token", "/?launch=guess"] {
            let response = get(uri).await;
            assert_eq!(response.status(), StatusCode::FORBIDDEN, "{uri}");
            assert_eq!(header_of(&response, "cache-control"), "no-store");
            assert!(header_of(&response, "content-security-policy").contains("default-src 'none'"));
            let html = body_text(response).await;
            assert!(html.contains("pulp ui"), "{uri}: {html}");
            assert!(!html.contains(TEST_TOKEN), "{uri} leaked the token");
            assert!(!html.contains("pulp-token"), "{uri} served the mill shell");
        }
    }

    #[tokio::test]
    async fn test_index_with_launch_code_redirects_once_to_token_link() {
        let state = AppState::new(TEST_TOKEN, TEST_ORIGIN);
        *state.launch.lock().unwrap() = Some(Launch {
            code: "0123456789abcdef".into(),
            until: Instant::now() + LAUNCH_TTL,
        });
        let app = router_with(state);
        let open = |uri: &'static str| {
            let app = app.clone();
            async move {
                send(
                    &app,
                    Request::builder()
                        .uri(uri)
                        .header(header::HOST, "127.0.0.1:8747")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
            }
        };
        let wrong = open("/?launch=fedcba9876543210").await;
        assert_eq!(wrong.status(), StatusCode::FORBIDDEN);
        let first = open("/?launch=0123456789abcdef").await;
        assert_eq!(first.status(), StatusCode::SEE_OTHER);
        assert_eq!(
            header_of(&first, "location"),
            format!("/?token={TEST_TOKEN}")
        );
        let again = open("/?launch=0123456789abcdef").await;
        assert_eq!(
            again.status(),
            StatusCode::FORBIDDEN,
            "the code works twice"
        );
    }

    #[tokio::test]
    async fn test_index_with_expired_launch_code_returns_locked_page() {
        let state = AppState::new(TEST_TOKEN, TEST_ORIGIN);
        *state.launch.lock().unwrap() = Some(Launch {
            code: "0123456789abcdef".into(),
            until: Instant::now() - Duration::from_millis(1),
        });
        let response = send(
            &router_with(state),
            Request::builder()
                .uri("/?launch=0123456789abcdef")
                .header(header::HOST, "127.0.0.1:8747")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn test_index_with_token_sends_script_nonce_policy_and_no_store() {
        let first = get_page().await;
        assert_eq!(first.status(), StatusCode::OK);
        assert_eq!(header_of(&first, "cache-control"), "no-store");
        assert_eq!(header_of(&first, "x-frame-options"), "SAMEORIGIN");
        assert_eq!(header_of(&first, "x-content-type-options"), "nosniff");
        assert_eq!(header_of(&first, "referrer-policy"), "no-referrer");
        assert_eq!(
            header_of(&first, "cross-origin-opener-policy"),
            "same-origin"
        );
        let csp = header_of(&first, "content-security-policy").to_string();
        for directive in [
            "default-src 'none'",
            "frame-ancestors 'self'",
            "connect-src 'self'",
            "base-uri 'none'",
            "form-action 'none'",
        ] {
            assert!(csp.contains(directive), "{csp} lacks {directive}");
        }
        let nonce = csp
            .split("'nonce-")
            .nth(1)
            .and_then(|rest| rest.split('\'').next())
            .unwrap()
            .to_string();
        assert_eq!(nonce.len(), 32);
        let html = body_text(first).await;
        let scripts = html.matches("<script").count();
        assert!(scripts >= 1);
        assert_eq!(
            html.matches(&format!("<script nonce=\"{nonce}\"")).count(),
            scripts,
            "every script tag must carry the page nonce"
        );
        let second = get_page().await;
        let again = header_of(&second, "content-security-policy").to_string();
        assert!(!again.contains(&nonce), "the nonce repeated across loads");
    }

    #[tokio::test]
    async fn test_api_responses_send_no_store_and_hardening_headers() {
        let app = router();
        let path = testdata().display().to_string();
        let pack = send(
            &app,
            Request::builder()
                .method("POST")
                .uri("/api/pack")
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::HOST, "127.0.0.1:8747")
                .header(header::ORIGIN, TEST_ORIGIN)
                .header("x-pulp-token", TEST_TOKEN)
                .body(Body::from(
                    serde_json::json!({
                        "path": path,
                        "gitignore": false,
                        "selected": ["hello.rs"]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;
        assert_eq!(pack.status(), StatusCode::OK);
        assert_eq!(header_of(&pack, "cache-control"), "no-store");
        assert_eq!(header_of(&pack, "x-content-type-options"), "nosniff");
        assert_eq!(
            header_of(&pack, "cross-origin-resource-policy"),
            "same-origin"
        );
        let json: serde_json::Value = serde_json::from_str(&body_text(pack).await).unwrap();
        let result_id = json["result_id"].as_str().unwrap();
        let artifact = get_authorized(&app, &format!("/api/artifact/{result_id}")).await;
        assert_eq!(artifact.status(), StatusCode::OK);
        assert_eq!(header_of(&artifact, "cache-control"), "no-store");
        assert_eq!(header_of(&artifact, "x-content-type-options"), "nosniff");
        assert_eq!(
            header_of(&artifact, "content-disposition"),
            "attachment; filename=\"pulp.xml\""
        );
        let denied = get("/api/progress").await;
        assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(header_of(&denied, "cache-control"), "no-store");
        // Assets keep their own cache policy and still refuse sniffing.
        let font = get("/fonts/plex-sans.woff2").await;
        assert_eq!(
            header_of(&font, "cache-control"),
            "public, max-age=31536000, immutable"
        );
        assert_eq!(header_of(&font, "x-content-type-options"), "nosniff");
    }

    #[tokio::test]
    async fn test_preflight_returns_refusal_without_cors_headers() {
        // A browser sends a preflight before a cross-origin call with the
        // token header; without CORS headers in the answer it never sends the call.
        for (uri, method) in [("/api/scan", "POST"), ("/api/artifact/x", "GET")] {
            let response = send(
                &router(),
                Request::builder()
                    .method("OPTIONS")
                    .uri(uri)
                    .header(header::HOST, "127.0.0.1:8747")
                    .header(header::ORIGIN, "http://evil.example")
                    .header("access-control-request-method", method)
                    .header("access-control-request-headers", "x-pulp-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;
            assert_eq!(response.status(), StatusCode::FORBIDDEN, "{uri}");
            assert!(
                response
                    .headers()
                    .keys()
                    .all(|name| !name.as_str().starts_with("access-control-")),
                "{uri}: {:?}",
                response.headers()
            );
        }
    }

    /// A scan of testdata with the mill's host, the token, and `headers`.
    async fn scan_with(headers: &[(&str, HeaderValue)]) -> StatusCode {
        let mut request = Request::builder()
            .method("POST")
            .uri("/api/scan")
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::HOST, "127.0.0.1:8747")
            .header("x-pulp-token", TEST_TOKEN);
        for (name, value) in headers {
            request = request.header(*name, value.clone());
        }
        let body = serde_json::json!({ "path": testdata().display().to_string() }).to_string();
        send(&router(), request.body(Body::from(body)).unwrap())
            .await
            .status()
    }

    #[tokio::test]
    async fn test_scan_with_undecodable_origin_returns_forbidden() {
        let origin = HeaderValue::from_bytes(b"http://127.0.0.1:8747\xff").unwrap();
        assert_eq!(
            scan_with(&[("origin", origin)]).await,
            StatusCode::FORBIDDEN
        );
    }

    #[tokio::test]
    async fn test_scan_with_cross_site_fetch_returns_forbidden() {
        for site in ["cross-site", "same-site", "none"] {
            assert_eq!(
                scan_with(&[("sec-fetch-site", HeaderValue::from_static(site))]).await,
                StatusCode::FORBIDDEN,
                "{site}"
            );
        }
        assert_eq!(
            scan_with(&[
                ("sec-fetch-site", HeaderValue::from_static("same-origin")),
                ("origin", HeaderValue::from_static(TEST_ORIGIN)),
            ])
            .await,
            StatusCode::OK
        );
    }

    #[test]
    fn test_same_secret_with_equal_and_different_secrets_returns_match() {
        assert!(same_secret(b"0123abcd", b"0123abcd"));
        assert!(!same_secret(b"0123abcd", b"0123abce"));
        assert!(!same_secret(b"x123abcd", b"0123abcd"));
        assert!(!same_secret(b"0123abc", b"0123abcd"));
        assert!(!same_secret(b"", b"0123abcd"));
    }

    #[tokio::test]
    async fn test_bind_localhost_with_port_zero_returns_bound_port() {
        let (listener, port) = bind_localhost(0, false).await.unwrap();
        assert_ne!(port, 0);
        assert_eq!(listener.local_addr().unwrap().port(), port);
        assert!(listener.local_addr().unwrap().ip().is_loopback());
    }

    #[tokio::test]
    async fn test_browse_with_picker_open_returns_conflict() {
        let state = AppState {
            pick: stub_pick_testdata,
            ..AppState::new(TEST_TOKEN, TEST_ORIGIN)
        };
        let open = PickerSlot::take(&state.picking).unwrap();
        let app = router_with(state);
        let (status, json) = post_to(&app, "/api/browse", serde_json::json!({})).await;
        assert_eq!(status, StatusCode::CONFLICT, "{json}");
        assert!(json["error"].as_str().unwrap().contains("already open"));
        drop(open);
        let (status, json) = post_to(&app, "/api/browse", serde_json::json!({})).await;
        assert_eq!(status, StatusCode::OK, "{json}");
    }

    fn stub_pick_panic() -> Result<Option<PathBuf>, String> {
        panic!("picker crashed at /Users/someone/secret");
    }

    #[tokio::test]
    async fn test_browse_with_panicking_picker_returns_internal_error_and_frees_picker() {
        let state = AppState {
            pick: stub_pick_panic,
            ..AppState::new(TEST_TOKEN, TEST_ORIGIN)
        };
        let picking = Arc::clone(&state.picking);
        let app = router_with(state);
        let (status, json) = post_to(&app, "/api/browse", serde_json::json!({})).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{json}");
        let message = json["error"].as_str().unwrap();
        assert!(!message.contains("secret"), "{message}");
        assert!(!picking.load(Ordering::SeqCst), "the picker stayed taken");
    }

    #[tokio::test]
    async fn test_artifact_and_render_with_unknown_result_return_not_found() {
        let app = router();
        let artifact = get_authorized(&app, "/api/artifact/0123456789abcdef").await;
        assert_eq!(artifact.status(), StatusCode::NOT_FOUND);
        let (status, json) = post_to(
            &app,
            "/api/render",
            serde_json::json!({
                "path": testdata().display().to_string(),
                "result_id": "0123456789abcdef"
            }),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{json}");
    }

    #[tokio::test]
    async fn test_pack_with_manifest_from_other_default_excludes_returns_rescan() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".env"), "API_KEY=secret\n").unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn a() {}\n").unwrap();
        let app = router();
        let path = dir.path().display().to_string();
        let (status, scan) = post_to(
            &app,
            "/api/scan",
            serde_json::json!({ "path": path, "hidden": true, "no_default_excludes": true }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{scan}");
        assert!(
            scan["files"]
                .as_array()
                .unwrap()
                .iter()
                .any(|f| f["relative"] == ".env")
        );
        // A pack with the default excludes back on must not reuse that scan.
        let (status, json) = post_to(
            &app,
            "/api/pack",
            serde_json::json!({
                "path": path,
                "hidden": true,
                "manifest_id": scan["manifest_id"],
                "selected": [".env", "a.rs"]
            }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{json}");
        assert_eq!(json["error"], "manifest settings changed; rescan");
    }

    #[tokio::test]
    async fn test_scan_with_work_slots_taken_waits_for_one() {
        let state = AppState::new(TEST_TOKEN, TEST_ORIGIN);
        let held = Arc::clone(&state.work)
            .acquire_many_owned(WORK_SLOTS as u32)
            .await
            .unwrap();
        let app = router_with(state);
        let path = testdata().display().to_string();
        let pending = tokio::spawn({
            let app = app.clone();
            async move { post_to(&app, "/api/scan", serde_json::json!({ "path": path })).await }
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!pending.is_finished(), "the scan ran without a free slot");
        // Progress and cancel never wait on the work slots.
        assert_eq!(
            get_authorized(&app, "/api/progress").await.status(),
            StatusCode::OK
        );
        drop(held);
        let (status, json) = pending.await.unwrap();
        assert_eq!(status, StatusCode::OK, "{json}");
    }

    /// Every API route, with the method the shell uses.
    const API_ROUTES: [(&str, &str); 10] = [
        ("POST", "/api/scan"),
        ("POST", "/api/pack"),
        ("POST", "/api/tree"),
        ("POST", "/api/preview"),
        ("POST", "/api/render"),
        ("POST", "/api/cancel"),
        ("GET", "/api/progress"),
        ("GET", "/api/artifact/0123456789abcdef"),
        ("POST", "/api/browse"),
        ("POST", "/api/sample"),
    ];

    #[tokio::test]
    async fn test_every_api_route_without_session_returns_refusal() {
        let state = AppState {
            pick: stub_pick_testdata,
            ..AppState::new(TEST_TOKEN, TEST_ORIGIN)
        };
        let app = router_with(state);
        for (method, uri) in API_ROUTES {
            let request = |token: Option<&str>, origin: &str| {
                let mut builder = Request::builder()
                    .method(method)
                    .uri(uri)
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::HOST, "127.0.0.1:8747")
                    .header(header::ORIGIN, origin);
                if let Some(token) = token {
                    builder = builder.header("x-pulp-token", token);
                }
                builder.body(Body::from("{}")).unwrap()
            };
            let missing = send(&app, request(None, TEST_ORIGIN)).await;
            assert_eq!(missing.status(), StatusCode::UNAUTHORIZED, "{method} {uri}");
            let wrong = send(&app, request(Some("test-tokem"), TEST_ORIGIN)).await;
            assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED, "{method} {uri}");
            let foreign = send(&app, request(Some(TEST_TOKEN), "http://evil.example")).await;
            assert_eq!(foreign.status(), StatusCode::FORBIDDEN, "{method} {uri}");
        }
    }

    /// A free port with at least 20 ports above it, held until the listener drops.
    fn held_port() -> std::net::TcpListener {
        loop {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            if listener.local_addr().unwrap().port() <= 65_000 {
                return listener;
            }
        }
    }

    #[tokio::test]
    async fn test_bind_localhost_with_busy_port_walks_to_next_free_port() {
        let held = held_port();
        let busy = held.local_addr().unwrap().port();
        let (listener, port) = bind_localhost(busy, true).await.unwrap();
        assert!(port > busy && port <= busy + 20, "{busy} -> {port}");
        assert_eq!(listener.local_addr().unwrap().port(), port);
        assert!(listener.local_addr().unwrap().ip().is_loopback());
    }

    #[tokio::test]
    async fn test_bind_localhost_with_busy_explicit_port_returns_error() {
        let held = held_port();
        let busy = held.local_addr().unwrap().port();
        let err = bind_localhost(busy, false).await.unwrap_err().to_string();
        assert!(
            err.contains(&format!("127.0.0.1:{busy} is already in use")),
            "{err}"
        );
    }
}
