//! `cargo xtask ui-test`: run the mill's browser tests in headless Chrome.
//!
//! A small static server maps the workspace the way the pages expect it:
//!
//! - `/web/<path>` serves `web/<path>`: the mill UI, and its tests in `web/test/`.
//! - Any other `/<path>` serves `site/<path>`, the layout the site deploys, so the
//!   site pages' `/pulp.css`, `/styles.css`, `/fonts/`, and `/mill/` resolve.
//!
//! A directory serves its `index.html`, so `/` is the landing page and `/mill/`
//! the browser mill, both unmodified.
//!
//! A page that declares `<meta name="ui-test-server" content="pulp-ui">` runs on
//! a second server instead, the mill origin: it serves `/web/` the same way and
//! forwards every other path to a real `pulp ui` built from this checkout, so
//! the page can open the local mill from the link pulp printed and drive it
//! same-origin (see [`mill`]).
//!
//! Every `web/test/*.test.html` page runs in its own headless Chrome with a
//! fresh profile. A page reports through `<pre id="results" data-status="…">`:
//! one `PASS name` or `FAIL name` line per test, failure detail indented by two
//! spaces, and `data-status` set to `pass` or `fail` once every test has run.
//!
//! `--dump-dom` prints the page when its load event fires. A test page holds
//! that event open with a hidden frame on `/__ui-test/hold`, which the server
//! answers once the page posts `/__ui-test/done` (or the page budget runs out).
//! Chrome's virtual time is not used: it stalls the worker the browser mill
//! packs in. Chrome can also stay up after printing, so it is stopped here as
//! soon as the dump is complete.
//!
//! Chrome, `pulp ui`, and the run's scratch folder (Chrome profiles, the mill
//! fixtures, pulp's temp files) are cleaned up when the run passes, fails, or
//! is stopped by Ctrl-C or SIGTERM.

use std::ffi::OsString;
use std::io::{self, BufRead, BufReader, ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::sync::{Arc, Condvar, Mutex, PoisonError, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, anyhow, bail};

use crate::host::Host;

mod mill;

/// Test pages live here, relative to the workspace root.
const TEST_DIR: &str = "web/test";
/// Longest a page may run before the server lets its load event fire.
const PAGE_BUDGET: Duration = Duration::from_secs(120);
/// Time on top of the budget for Chrome to start and print the page.
const CHROME_GRACE: Duration = Duration::from_secs(30);
/// Time Chrome gets to close its helper processes after SIGTERM.
const CHROME_STOP_GRACE: Duration = Duration::from_secs(3);
const HOLD_PATH: &str = "/__ui-test/hold";
const DONE_PATH: &str = "/__ui-test/done";
/// The mill origin's folders for its test page, as JSON.
const ENV_PATH: &str = "/__ui-test/env";
/// Restarts `pulp ui` behind the mill origin.
const RESTART_PATH: &str = "/__ui-test/restart";
/// Holds the mill's progress polls, and lets them go.
const HOLD_POLLS_PATH: &str = "/__ui-test/hold-polls";
const RELEASE_POLLS_PATH: &str = "/__ui-test/release-polls";
/// A page's `<meta name=…>` that picks its server.
const SERVER_META: &str = "ui-test-server";
const MAC_CHROME: &str = "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
/// Browser names tried on `PATH`, in order.
const PATH_CHROMES: &[&str] = &[
    "google-chrome",
    "google-chrome-stable",
    "chromium",
    "chromium-browser",
];
/// Request line plus headers; anything longer is not a browser request.
const MAX_HEAD: u64 = 64 * 1024;
/// Largest request body read; the mill's largest is a pack of many file ids.
const MAX_BODY: u64 = 16 * 1024 * 1024;

/// Set by Ctrl-C or SIGTERM during a run, so Chrome, `pulp ui`, and the scratch
/// folder are cleaned up instead of left behind: headless Chrome does not exit
/// on SIGINT, and `pulp ui` does not see a SIGTERM sent to this process.
static INTERRUPTED: AtomicBool = AtomicBool::new(false);

fn interrupted() -> bool {
    INTERRUPTED.load(Ordering::SeqCst)
}

/// Run every test page, or with `serve_only` keep serving for a browser.
pub fn run(root: &Path, host: &Host, serve_only: bool) -> anyhow::Result<()> {
    catch_interrupts();
    let pages = test_pages(root)?;
    // Look for Chrome before building pulp, so a missing browser fails fast.
    let chrome = if serve_only {
        None
    } else {
        Some(find_chrome(
            std::env::var_os("PULP_CHROME"),
            std::env::var_os("PATH"),
            cfg!(target_os = "macos"),
        )?)
    };

    // Guards drop in reverse order: `pulp ui` stops before the scratch folder
    // that holds its temp files is removed.
    let scratch = Scratch::create()?;
    let gate = Arc::new(Gate::default());
    let site = Server::start(root, None, &gate)?;
    let mill = if pages.iter().any(|page| page.behind == Behind::PulpUi) {
        let running = mill::Mill::start(root, host, scratch.path())?;
        let server = Server::start(root, Some(Arc::clone(running.mill())), &gate)?;
        eprintln!(
            "xtask ui-test: pulp ui on http://{}, behind {}",
            running.mill().upstream(),
            server.url("/")
        );
        Some((server, running))
    } else {
        None
    };
    let server_for = |page: &Page| match page.behind {
        Behind::Site => Some(&site),
        Behind::PulpUi => mill.as_ref().map(|(server, _)| server),
    };

    let Some(chrome) = chrome else {
        let mill = mill
            .as_ref()
            .map(|(server, running)| (server, running.mill().as_ref()));
        serve_until_interrupted(&pages, &site, mill);
        return Ok(());
    };
    eprintln!("xtask ui-test: {} on {}", chrome.display(), site.url("/"));

    let started = Instant::now();
    let (mut passed, mut failed, mut broken) = (0, 0, 0);
    for (index, page) in pages.iter().enumerate() {
        let server = server_for(page).context("no server for a pulp ui page")?;
        match page.behind {
            Behind::Site => println!("{TEST_DIR}/{}", page.name),
            Behind::PulpUi => println!("{TEST_DIR}/{} (real pulp ui)", page.name),
        }
        gate.set(false);
        let url = server.url(&format!("/{TEST_DIR}/{}", page.name));
        let dom = dump_dom(
            &chrome,
            &url,
            &scratch.path().join(index.to_string()),
            PAGE_BUDGET + CHROME_GRACE,
        );
        if interrupted() {
            bail!("ui-test interrupted; Chrome and pulp ui stopped, scratch files removed");
        }
        let clean = match dom.map(|dom| parse_results(&dom)) {
            Err(err) => {
                broken += 1;
                println!("  FAIL {err:#}");
                false
            }
            Ok(None) => {
                broken += 1;
                println!("  FAIL no results block; the test page failed before it could report");
                false
            }
            Ok(Some(results)) => {
                print_results(&results);
                passed += results.passed();
                failed += results.failed();
                let problem = page_problem(&results);
                if let Some(problem) = &problem {
                    broken += 1;
                    println!("  FAIL {problem}");
                }
                results.is_pass() && problem.is_none()
            }
        };
        if !clean && let (Behind::PulpUi, Some((_, running))) = (page.behind, &mill) {
            println!("  pulp ui log:");
            for line in running.mill().log().lines() {
                println!("    {line}");
            }
        }
    }

    let secs = started.elapsed().as_secs_f64();
    eprintln!("xtask ui-test: {passed} passed, {failed} failed in {secs:.1}s");
    if failed > 0 || broken > 0 {
        bail!("ui-test failed: {failed} failing test(s), {broken} page(s) without a clean result");
    }
    Ok(())
}

/// Turn Ctrl-C and SIGTERM into a flag the run checks, so it can stop Chrome
/// and `pulp ui` and remove the scratch folder before exiting. Without a
/// handler the process would die at once and leave them behind.
fn catch_interrupts() {
    for signal in [libc::SIGINT, libc::SIGTERM] {
        // SAFETY: the action only stores to an atomic, which is async-signal-safe.
        let registered = unsafe {
            signal_hook_registry::register(signal, || INTERRUPTED.store(true, Ordering::SeqCst))
        };
        if let Err(err) = registered {
            eprintln!("xtask ui-test: cannot catch signal {signal}: {err}");
        }
    }
}

/// The run's private scratch folder, removed when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn create() -> anyhow::Result<Self> {
        private_scratch_dir().map(Self)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        remove_dir_eventually(&self.0);
    }
}

/// A fresh owner-only folder for Chrome profiles. The name is random, and the
/// create fails if it exists, so nothing planted in a shared temp folder is used.
pub(crate) fn private_scratch_dir() -> anyhow::Result<PathBuf> {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};

    let temp = std::env::temp_dir();
    for _ in 0..8 {
        // RandomState is seeded from the OS, so the name cannot be guessed.
        let mut hasher = RandomState::new().build_hasher();
        hasher.write_u32(std::process::id());
        let dir = temp.join(format!("pulp-ui-test-{:016x}", hasher.finish()));
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        match builder.create(&dir) {
            Ok(()) => return Ok(dir),
            Err(err) if err.kind() == ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err).with_context(|| format!("create {}", dir.display())),
        }
    }
    bail!("could not create a scratch folder in {}", temp.display())
}

/// Serve until Ctrl-C or SIGTERM, then return so the caller cleans up.
fn serve_until_interrupted(pages: &[Page], site: &Server, mill: Option<(&Server, &mill::Mill)>) {
    // Nobody reads the dump here, so pages load without waiting on the gate.
    site.gate.set(true);
    eprintln!("xtask ui-test: serving web/ and site/ at {}", site.url("/"));
    for page in pages {
        match (page.behind, mill) {
            (Behind::PulpUi, Some((mill, _))) => eprintln!(
                "  {}  against pulp ui",
                mill.url(&format!("/{TEST_DIR}/{}", page.name))
            ),
            _ => eprintln!("  {}", site.url(&format!("/{TEST_DIR}/{}", page.name))),
        }
    }
    eprintln!("  {}  landing page", site.url("/"));
    eprintln!("  {}  browser mill", site.url("/mill/"));
    if let Some((server, mill)) = mill {
        eprintln!("  {}  local mill, a real pulp ui", server.url(&mill.page()));
    }
    eprintln!("Ctrl-C stops the servers.");
    while !interrupted() {
        thread::sleep(Duration::from_millis(100));
    }
}

/// A test page and the server it runs on.
#[derive(Debug, PartialEq, Eq)]
struct Page {
    /// File name under `web/test/`.
    name: String,
    behind: Behind,
}

/// `*.test.html` pages under `web/test/`, sorted by name.
fn test_pages(root: &Path) -> anyhow::Result<Vec<Page>> {
    let dir = root.join(TEST_DIR);
    let entries = std::fs::read_dir(&dir).with_context(|| format!("read {}", dir.display()))?;
    let mut names: Vec<String> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| name.ends_with(".test.html"))
        .collect();
    names.sort();
    if names.is_empty() {
        bail!("no *.test.html pages in {}", dir.display());
    }
    names
        .into_iter()
        .map(|name| {
            let path = dir.join(&name);
            let html = std::fs::read_to_string(&path)
                .with_context(|| format!("read {}", path.display()))?;
            let behind = page_server(&html).with_context(|| format!("{TEST_DIR}/{name}"))?;
            Ok(Page { name, behind })
        })
        .collect()
}

/// The server a page asks for with `<meta name="ui-test-server" content="…">`,
/// written with double quotes; the static site server when it asks for none.
fn page_server(html: &str) -> anyhow::Result<Behind> {
    let mut from = 0;
    while let Some(found) = html[from..].find("<meta") {
        let start = from + found;
        let Some(len) = html[start..].find('>') else {
            break;
        };
        let tag = &html[start..start + len];
        from = start + len;
        if attr(tag, "name").as_deref() != Some(SERVER_META) {
            continue;
        }
        return match attr(tag, "content").as_deref() {
            Some("pulp-ui") => Ok(Behind::PulpUi),
            other => bail!("unknown {SERVER_META} {other:?}; the one choice is \"pulp-ui\""),
        };
    }
    Ok(Behind::Site)
}

/* ---------- server ---------- */

/// What answers the requests a server does not handle itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Behind {
    /// Files from `site/`, laid out as the site deploys.
    Site,
    /// A running `pulp ui`, reached through [`mill::Mill::forward`].
    PulpUi,
}

/// Serves `web/`, and `site/` or a forward to `pulp ui`, on an ephemeral
/// 127.0.0.1 port until the process exits.
struct Server {
    addr: SocketAddr,
    gate: Arc<Gate>,
}

impl Server {
    /// Start a server; with a `mill` it forwards to `pulp ui` instead of serving `site/`.
    fn start(root: &Path, mill: Option<Arc<mill::Mill>>, gate: &Arc<Gate>) -> anyhow::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").context("bind the ui-test server")?;
        let addr = listener.local_addr()?;
        let root = root.to_path_buf();
        let shared = Arc::clone(gate);
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let root = root.clone();
                let gate = Arc::clone(&shared);
                let mill = mill.clone();
                thread::spawn(move || {
                    // A browser that drops a connection early is not a test failure.
                    let _ = handle(stream, &root, &gate, mill.as_deref(), addr);
                });
            }
        });
        Ok(Self {
            addr,
            gate: Arc::clone(gate),
        })
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }
}

/// Holds a test page's load event until the page says it is done.
#[derive(Default)]
struct Gate {
    open: Mutex<bool>,
    changed: Condvar,
}

impl Gate {
    fn set(&self, open: bool) {
        *self.open.lock().unwrap_or_else(PoisonError::into_inner) = open;
        self.changed.notify_all();
    }

    fn wait(&self, budget: Duration) {
        let open = self.open.lock().unwrap_or_else(PoisonError::into_inner);
        let _ = self
            .changed
            .wait_timeout_while(open, budget, |open| !*open)
            .unwrap_or_else(PoisonError::into_inner);
    }
}

/// What the server does with one request.
#[derive(Debug, PartialEq, Eq)]
enum Reply {
    File(PathBuf),
    Hold,
    Done,
    /// The mill origin's fixtures, as JSON.
    Env,
    /// Restart `pulp ui`.
    Restart,
    /// Hold progress polls until one is held.
    HoldPolls,
    /// Let progress polls through.
    ReleasePolls,
    /// Send the request on to `pulp ui`.
    Forward,
    Status(u16),
}

fn route(root: &Path, behind: Behind, method: &str, target: &str) -> Reply {
    let path = target.split(['?', '#']).next().unwrap_or_default();
    let harness = path == "/web" || path.starts_with("/web/") || path.starts_with("/__ui-test/");
    match (method, path) {
        ("GET", HOLD_PATH) => Reply::Hold,
        ("POST", DONE_PATH) => Reply::Done,
        ("GET", ENV_PATH) if behind == Behind::PulpUi => Reply::Env,
        ("POST", RESTART_PATH) if behind == Behind::PulpUi => Reply::Restart,
        ("POST", HOLD_POLLS_PATH) if behind == Behind::PulpUi => Reply::HoldPolls,
        ("POST", RELEASE_POLLS_PATH) if behind == Behind::PulpUi => Reply::ReleasePolls,
        _ if behind == Behind::PulpUi && !harness => Reply::Forward,
        ("GET" | "HEAD", _) => match resolve(root, target) {
            Some(file) if file.is_dir() => Reply::File(file.join("index.html")),
            Some(file) => Reply::File(file),
            None => Reply::Status(404),
        },
        _ => Reply::Status(405),
    }
}

/// Map a request target to a path under `root`, or `None` when it is malformed
/// or would leave the served trees. Dot segments and hidden names never map.
fn resolve(root: &Path, target: &str) -> Option<PathBuf> {
    let path = target.split(['?', '#']).next().unwrap_or_default();
    let path = percent_decode(path)?;
    let rest = path.strip_prefix('/')?;
    let (tree, rest) = match rest.split_once('/') {
        Some(("web", rest)) => ("web", rest),
        _ if rest == "web" => ("web", ""),
        _ => ("site", rest),
    };
    let mut file = root.join(tree);
    for segment in rest.split('/').filter(|s| !s.is_empty()) {
        if segment.starts_with('.') || segment.contains(['\\', ':', '\0']) {
            return None;
        }
        file.push(segment);
    }
    Some(file)
}

/// Decode `%XX` escapes; `None` for a bad escape or bytes that are not UTF-8.
fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = bytes.get(i + 1..i + 3)?;
            if !hex.iter().all(u8::is_ascii_hexdigit) {
                return None;
            }
            out.push(u8::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|ext| ext.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("js" | "mjs") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json") => "application/json",
        Some("wasm") => "application/wasm",
        Some("woff2") => "font/woff2",
        Some("md") => "text/markdown; charset=utf-8",
        Some("txt") => "text/plain; charset=utf-8",
        Some("xml") => "application/xml",
        Some("svg") => "image/svg+xml",
        _ => "application/octet-stream",
    }
}

/// Answer one request. `mill` is set on the mill origin, whose own address is `own`.
fn handle(
    stream: TcpStream,
    root: &Path,
    gate: &Gate,
    mill: Option<&mill::Mill>,
    own: SocketAddr,
) -> io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let head = read_head(&mut (&mut reader).take(MAX_HEAD))?;
    let head_only = head.method == "HEAD";
    if head.header("transfer-encoding").is_some() {
        return respond(stream, 501, "text/plain", b"send a Content-Length\n", false);
    }
    if head.content_length() > MAX_BODY {
        return respond(stream, 413, "text/plain", b"request too large\n", false);
    }
    // Read the whole body, so it can be forwarded and so closing the socket
    // does not reset the browser's request.
    let mut body = Vec::new();
    (&mut reader)
        .take(head.content_length())
        .read_to_end(&mut body)?;

    let behind = if mill.is_some() {
        Behind::PulpUi
    } else {
        Behind::Site
    };
    // The mill origin's own pages carry pulp's opener policy, so a test page
    // keeps its handle on the mill window it opens.
    let extra = mill
        .and_then(mill::Mill::opener_policy)
        .map(|policy| format!("Cross-Origin-Opener-Policy: {policy}\r\n"))
        .unwrap_or_default();
    let answer = |stream, status, content_type: &str, body: &[u8], head_only| {
        respond_with(stream, status, content_type, body, head_only, &extra)
    };
    match (route(root, behind, &head.method, &head.target), mill) {
        (Reply::File(file), _) => match std::fs::read(&file) {
            Ok(body) => answer(stream, 200, content_type(&file), &body, head_only),
            Err(_) => answer(stream, 404, "text/plain", b"not found\n", head_only),
        },
        (Reply::Hold, _) => {
            gate.wait(PAGE_BUDGET);
            answer(stream, 200, "text/html; charset=utf-8", b"", false)
        }
        (Reply::Done, _) => {
            gate.set(true);
            answer(stream, 200, "text/plain", b"done\n", false)
        }
        (Reply::Env, Some(mill)) => answer(
            stream,
            200,
            "application/json",
            mill.env().as_bytes(),
            false,
        ),
        (Reply::Restart, Some(mill)) => match mill.restart() {
            Ok(()) => answer(
                stream,
                200,
                "application/json",
                b"{\"restarted\":true}",
                false,
            ),
            Err(err) => answer(
                stream,
                500,
                "text/plain; charset=utf-8",
                format!("{err:#}\n").as_bytes(),
                false,
            ),
        },
        (Reply::HoldPolls, Some(mill)) => {
            let held = mill.hold_polls(Duration::from_secs(10));
            let json = format!("{{\"held\":{held}}}");
            answer(stream, 200, "application/json", json.as_bytes(), false)
        }
        (Reply::ReleasePolls, Some(mill)) => {
            mill.release_polls();
            answer(
                stream,
                200,
                "application/json",
                b"{\"released\":true}",
                false,
            )
        }
        (Reply::Forward, Some(mill)) => mill.forward(stream, &head, &body, own),
        (
            Reply::Env | Reply::Restart | Reply::HoldPolls | Reply::ReleasePolls | Reply::Forward,
            None,
        ) => answer(stream, 404, "text/plain", b"not found\n", head_only),
        (Reply::Status(code), _) => {
            let body: &[u8] = if code == 404 {
                b"not found\n"
            } else {
                b"method not allowed\n"
            };
            answer(stream, code, "text/plain", body, head_only)
        }
    }
}

/// A request's first line and headers.
#[derive(Debug)]
struct Head {
    method: String,
    target: String,
    /// Header names and values in the order sent, names as the client wrote them.
    headers: Vec<(String, String)>,
}

impl Head {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    fn content_length(&self) -> u64 {
        self.header("content-length")
            .and_then(|value| value.trim().parse().ok())
            .unwrap_or(0)
    }
}

/// Read the request line and headers, up to the blank line that ends them.
fn read_head(head: &mut impl BufRead) -> io::Result<Head> {
    let mut line = String::new();
    head.read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let target = parts.next().unwrap_or_default().to_string();
    let mut headers = Vec::new();
    loop {
        let mut line = String::new();
        if head.read_line(&mut line)? == 0 {
            break;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_string(), value.trim().to_string()));
        }
    }
    Ok(Head {
        method,
        target,
        headers,
    })
}

fn respond(
    stream: TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
    head_only: bool,
) -> io::Result<()> {
    respond_with(stream, status, content_type, body, head_only, "")
}

/// [`respond`] with `extra` header lines, each ending in CRLF.
fn respond_with(
    mut stream: TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
    head_only: bool,
    extra: &str,
) -> io::Result<()> {
    let reason = match status {
        200 => "OK",
        404 => "Not Found",
        405 => "Method Not Allowed",
        413 => "Content Too Large",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        502 => "Bad Gateway",
        _ => "Error",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\
         Cache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\n{extra}Connection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    if !head_only {
        stream.write_all(body)?;
    }
    stream.flush()
}

/* ---------- Chrome ---------- */

/// `PULP_CHROME` (a path, or a name on `PATH`), then the macOS app, then the
/// usual Chrome and Chromium names on `PATH`.
fn find_chrome(
    env_chrome: Option<OsString>,
    path_var: Option<OsString>,
    macos: bool,
) -> anyhow::Result<PathBuf> {
    let dirs: Vec<PathBuf> = path_var
        .map(|paths| std::env::split_paths(&paths).collect())
        .unwrap_or_default();
    let on_path = |name: &Path| {
        let mut exe = name.as_os_str().to_owned();
        exe.push(std::env::consts::EXE_SUFFIX);
        dirs.iter().map(|dir| dir.join(&exe)).find(|p| p.is_file())
    };
    if let Some(chrome) = env_chrome.filter(|value| !value.is_empty()) {
        let chrome = PathBuf::from(chrome);
        if chrome.is_file() {
            return Ok(chrome);
        }
        return on_path(&chrome)
            .ok_or_else(|| anyhow!("PULP_CHROME is {}, which is not a file", chrome.display()));
    }
    if macos && Path::new(MAC_CHROME).is_file() {
        return Ok(MAC_CHROME.into());
    }
    PATH_CHROMES
        .iter()
        .find_map(|name| on_path(Path::new(name)))
        .ok_or_else(|| {
            anyhow!(
                "no Chrome or Chromium found (tried {}); install one or set PULP_CHROME to the browser binary",
                PATH_CHROMES.join(", ")
            )
        })
}

fn chrome_args(url: &str, profile: &Path) -> Vec<OsString> {
    let mut profile_arg = OsString::from("--user-data-dir=");
    profile_arg.push(profile);
    let mut args: Vec<OsString> = [
        "--headless=new",
        "--disable-gpu",
        "--no-first-run",
        "--no-default-browser-check",
        "--disable-extensions",
        "--disable-background-networking",
        "--disable-component-update",
        "--use-mock-keychain",
        "--window-size=1440,1000",
        // The local mill page opens the shell in windows of its own; neither
        // those windows nor the page behind them get slowed timers.
        "--disable-popup-blocking",
        "--disable-background-timer-throttling",
        "--disable-backgrounding-occluded-windows",
        "--disable-renderer-backgrounding",
    ]
    .into_iter()
    .map(OsString::from)
    .collect();
    args.push(profile_arg);
    if cfg!(target_os = "linux") {
        // CI runners and containers usually cannot start Chrome's sandbox.
        args.push("--no-sandbox".into());
        args.push("--disable-dev-shm-usage".into());
    }
    args.push("--dump-dom".into());
    args.push(url.into());
    args
}

/// Load `url` in headless Chrome and return the DOM it prints.
fn dump_dom(chrome: &Path, url: &str, profile: &Path, timeout: Duration) -> anyhow::Result<String> {
    let mut command = Command::new(chrome);
    command
        .args(chrome_args(url, profile))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // A process group of its own, so stopping Chrome reaches every helper
        // it started. The harness stops it on Ctrl-C itself.
        command.process_group(0);
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("start {}", chrome.display()))?;
    let stdout = child.stdout.take().context("capture Chrome's stdout")?;
    let stderr = child.stderr.take().context("capture Chrome's stderr")?;
    let (dom_tx, dom_rx) = mpsc::channel();
    thread::spawn(move || dom_tx.send(read_dump(stdout)));
    let (err_tx, err_rx) = mpsc::channel();
    thread::spawn(move || err_tx.send(read_tail(stderr, 4096)));

    // Wait in short slices so an interrupt stops Chrome promptly.
    let deadline = Instant::now() + timeout;
    let dom = loop {
        if interrupted() {
            stop_group(&mut child, CHROME_STOP_GRACE);
            bail!("interrupted");
        }
        match dom_rx.recv_timeout(Duration::from_millis(100)) {
            Err(RecvTimeoutError::Timeout) if Instant::now() < deadline => {}
            other => break other,
        }
    };
    stop_group(&mut child, CHROME_STOP_GRACE);
    match dom {
        Ok(dom) if !dom.trim().is_empty() => Ok(dom),
        Ok(_) => {
            let log = err_rx
                .recv_timeout(Duration::from_secs(5))
                .unwrap_or_default();
            bail!("Chrome printed nothing; its log ends with:\n{log}")
        }
        Err(_) => bail!("Chrome printed nothing within {}s", timeout.as_secs()),
    }
}

/// Stop a child process: SIGTERM, then a kill once `grace` runs out. SIGTERM
/// lets `pulp ui` finish open requests and remove its sample folder.
fn stop(child: &mut Child, grace: Duration) {
    #[cfg(unix)]
    {
        // The child is not reaped before this, so its pid is still its own.
        let asked = libc::pid_t::try_from(child.id())
            // SAFETY: kill(2) takes two integers and touches no memory of ours.
            .is_ok_and(|pid| unsafe { libc::kill(pid, libc::SIGTERM) } == 0);
        let deadline = Instant::now() + grace;
        while asked && Instant::now() < deadline {
            if let Ok(Some(_)) = child.try_wait() {
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }
    }
    #[cfg(not(unix))]
    let _ = grace;
    let _ = child.kill();
    let _ = child.wait();
}

/// Stop a child that leads its own process group, and every process in it.
/// SIGTERM lets Chrome close its helpers; once it has exited, or `grace` has
/// run out, SIGKILL goes to the whole group, since a helper busy with a page
/// can outlive the browser by many seconds. The group is killed before the
/// child is reaped, while its pid, and so the group id, cannot be reused.
fn stop_group(child: &mut Child, grace: Duration) {
    #[cfg(unix)]
    if let Ok(pid) = libc::pid_t::try_from(child.id()) {
        // SAFETY: kill(2) takes two integers and touches no memory of ours.
        let asked = unsafe { libc::kill(pid, libc::SIGTERM) } == 0;
        let deadline = Instant::now() + grace;
        while asked && !has_exited(pid) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        // SAFETY: as above; a negative pid names the process group.
        unsafe { libc::kill(-pid, libc::SIGKILL) };
    }
    #[cfg(not(unix))]
    let _ = grace;
    let _ = child.kill();
    let _ = child.wait();
}

/// Whether the child `pid` has exited. An exited child is left unreaped, so
/// its pid and the process group it leads stay reserved.
#[cfg(unix)]
fn has_exited(pid: libc::pid_t) -> bool {
    let Ok(id) = libc::id_t::try_from(pid) else {
        return true;
    };
    // SAFETY: siginfo_t is plain data, for which all zeroes is a valid value.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let flags = libc::WEXITED | libc::WNOHANG | libc::WNOWAIT;
    // SAFETY: waitid writes only to `info`, which outlives the call.
    let found = unsafe { libc::waitid(libc::P_PID, id, &mut info, flags) };
    // A child still running leaves `info` zeroed; an error means no such child.
    found != 0 || info.si_signo != 0
}

/// Read Chrome's output until the document ends; Chrome may not exit after it.
fn read_dump(mut out: impl Read) -> String {
    let mut dom = Vec::new();
    let mut chunk = [0u8; 16 * 1024];
    loop {
        match out.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                dom.extend_from_slice(&chunk[..n]);
                if dom.trim_ascii_end().ends_with(b"</html>") {
                    break;
                }
            }
            Err(err) if err.kind() == ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    String::from_utf8_lossy(&dom).into_owned()
}

/// The last `keep` bytes of a stream, for error messages.
fn read_tail(mut out: impl Read, keep: usize) -> String {
    let mut all = Vec::new();
    let _ = out.read_to_end(&mut all);
    let start = all.len().saturating_sub(keep);
    String::from_utf8_lossy(&all[start..]).into_owned()
}

/// Chrome's helpers can hold the profile open briefly after the browser exits.
fn remove_dir_eventually(dir: &Path) {
    for _ in 0..20 {
        if std::fs::remove_dir_all(dir).is_ok() || !dir.exists() {
            return;
        }
        thread::sleep(Duration::from_millis(100));
    }
}

/* ---------- results ---------- */

/// One test line from a page's results block.
#[derive(Debug, Clone, PartialEq, Eq)]
struct TestLine {
    passed: bool,
    name: String,
    detail: Vec<String>,
}

impl TestLine {
    fn new(passed: bool, name: &str) -> Self {
        Self {
            passed,
            name: name.trim().to_string(),
            detail: Vec::new(),
        }
    }
}

/// A page's `<pre id="results">` block.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Results {
    /// `pass`, `fail`, or `running` when the page never finished.
    status: String,
    tests: Vec<TestLine>,
    /// Other non-empty lines, such as the page's summary.
    notes: Vec<String>,
}

impl Results {
    fn passed(&self) -> usize {
        self.tests.iter().filter(|t| t.passed).count()
    }

    fn failed(&self) -> usize {
        self.tests.iter().filter(|t| !t.passed).count()
    }

    /// Finished, ran at least one test, and every test passed.
    fn is_pass(&self) -> bool {
        self.status == "pass" && !self.tests.is_empty() && self.failed() == 0
    }
}

/// What is wrong with a page beyond its failing tests, if anything.
fn page_problem(results: &Results) -> Option<String> {
    match results.status.as_str() {
        "pass" | "fail" if results.tests.is_empty() => Some("the page ran no tests".into()),
        "pass" | "fail" if !results.is_pass() && results.failed() == 0 => {
            Some("the page reported a failure without a failing test".into())
        }
        "pass" | "fail" => None,
        status => Some(format!(
            "the page was still {status:?} when Chrome printed it; a test page holds its load \
             event on {HOLD_PATH} until it posts {DONE_PATH}, for at most {}s",
            PAGE_BUDGET.as_secs()
        )),
    }
}

fn print_results(results: &Results) {
    for test in &results.tests {
        println!(
            "  {} {}",
            if test.passed { "PASS" } else { "FAIL" },
            test.name
        );
        for line in &test.detail {
            println!("       {line}");
        }
    }
    for note in &results.notes {
        println!("  {note}");
    }
}

/// Find `<pre id="results">` in a dumped DOM and read its lines.
fn parse_results(dom: &str) -> Option<Results> {
    let mut from = 0;
    while let Some(found) = dom[from..].find("<pre") {
        let start = from + found;
        let tag_end = start + dom[start..].find('>')?;
        let tag = &dom[start..tag_end];
        from = tag_end;
        if attr(tag, "id").as_deref() != Some("results") {
            continue;
        }
        let body = &dom[tag_end + 1..];
        let body = &body[..body.find("</pre>")?];
        let status = attr(tag, "data-status").unwrap_or_default();
        return Some(parse_lines(&unescape(body), status));
    }
    None
}

fn parse_lines(body: &str, status: String) -> Results {
    let mut results = Results {
        status,
        tests: Vec::new(),
        notes: Vec::new(),
    };
    for line in body.lines() {
        if let Some(name) = line.strip_prefix("PASS ") {
            results.tests.push(TestLine::new(true, name));
        } else if let Some(name) = line.strip_prefix("FAIL ") {
            results.tests.push(TestLine::new(false, name));
        } else if line.trim().is_empty() {
            continue;
        } else if let Some(last) = results.tests.last_mut().filter(|_| line.starts_with("  ")) {
            last.detail.push(line.trim().to_string());
        } else {
            results.notes.push(line.trim().to_string());
        }
    }
    results
}

/// The value of `name="…"` in a start tag as Chrome serializes it.
fn attr(tag: &str, name: &str) -> Option<String> {
    let needle = format!(" {name}=\"");
    let start = tag.find(&needle)? + needle.len();
    let len = tag[start..].find('"')?;
    Some(unescape(&tag[start..start + len]))
}

/// Undo the entity escapes of HTML serialization.
fn unescape(text: &str) -> String {
    const ENTITIES: &[(&str, &str)] = &[
        ("&amp;", "&"),
        ("&lt;", "<"),
        ("&gt;", ">"),
        ("&quot;", "\""),
        ("&#39;", "'"),
        ("&nbsp;", "\u{a0}"),
    ];
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        match ENTITIES.iter().find(|(entity, _)| rest.starts_with(entity)) {
            Some((entity, plain)) => {
                out.push_str(plain);
                rest = &rest[entity.len()..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Serializes the tests that run code which checks [`INTERRUPTED`], since one
/// of them sets it.
#[cfg(test)]
fn interrupt_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> PathBuf {
        crate::cargo::workspace_root()
    }

    /// `rel` (slash-separated) under `base`, joined the way the server joins it.
    fn under(base: &Path, rel: &str) -> PathBuf {
        rel.split('/')
            .fold(base.to_path_buf(), |path, part| path.join(part))
    }

    #[test]
    fn test_resolve_with_web_prefix_maps_into_web() {
        let root = Path::new("/w");
        let at = |rel| Some(under(root, rel));
        assert_eq!(resolve(root, "/web/mill.js"), at("web/mill.js"));
        assert_eq!(
            resolve(root, "/web/test/mill.test.html"),
            at("web/test/mill.test.html")
        );
        assert_eq!(resolve(root, "/web"), at("web"));
        assert_eq!(resolve(root, "/web/"), at("web"));
    }

    #[test]
    fn test_resolve_with_site_paths_maps_into_site() {
        let root = Path::new("/w");
        let at = |rel| Some(under(root, rel));
        assert_eq!(resolve(root, "/"), at("site"));
        assert_eq!(resolve(root, "/pulp.css"), at("site/pulp.css"));
        assert_eq!(resolve(root, "/styles.css"), at("site/styles.css"));
        assert_eq!(resolve(root, "/mill/"), at("site/mill"));
        assert_eq!(
            resolve(root, "/mill/pkg/pulp_wasm_bg.wasm"),
            at("site/mill/pkg/pulp_wasm_bg.wasm")
        );
        assert_eq!(resolve(root, "/webfonts/a.css"), at("site/webfonts/a.css"));
    }

    #[test]
    fn test_resolve_with_query_and_escapes_decodes_the_path() {
        let root = Path::new("/w");
        let at = |rel| Some(under(root, rel));
        assert_eq!(
            resolve(root, "/fonts/plex-mono-400.woff2?v=2"),
            at("site/fonts/plex-mono-400.woff2")
        );
        assert_eq!(resolve(root, "/mill/#top"), at("site/mill"));
        assert_eq!(
            resolve(root, "/web/test/a%20b.html"),
            at("web/test/a b.html")
        );
    }

    #[test]
    fn test_resolve_with_traversal_returns_none() {
        let root = Path::new("/w");
        for target in [
            "/../Cargo.toml",
            "/web/../Cargo.toml",
            "/web/test/../../Cargo.toml",
            "/%2e%2e/Cargo.toml",
            "/%2E%2E%2FCargo.toml",
            "/web/%2e%2e%2fsrc%2fmain.rs",
            "/..%5c..%5cCargo.toml",
            "/mill/..",
            "/mill/./index.html",
            "/.git/config",
            "/web/.hidden",
            "/C:/Windows/win.ini",
            "/mill/%00index.html",
            "/mill/%zz",
            "/mill/%e",
            "/mill/%ff%fe",
            "http://127.0.0.1/pulp.css",
            "",
        ] {
            assert_eq!(resolve(root, target), None, "{target} must not map");
        }
    }

    #[test]
    fn test_route_with_directory_serves_its_index() {
        let root = root();
        let file = |rel| Reply::File(under(&root, rel));
        assert_eq!(
            route(&root, Behind::Site, "GET", "/"),
            file("site/index.html")
        );
        assert_eq!(
            route(&root, Behind::Site, "GET", "/mill"),
            file("site/mill/index.html")
        );
        assert_eq!(
            route(&root, Behind::Site, "GET", "/mill/"),
            file("site/mill/index.html")
        );
        assert_eq!(
            route(&root, Behind::Site, "HEAD", "/web/mill.js"),
            file("web/mill.js")
        );
    }

    #[test]
    fn test_route_with_gate_and_bad_requests_returns_matching_replies() {
        let root = root();
        assert_eq!(
            route(&root, Behind::Site, "GET", "/__ui-test/hold"),
            Reply::Hold
        );
        assert_eq!(
            route(&root, Behind::Site, "POST", "/__ui-test/done"),
            Reply::Done
        );
        assert_eq!(
            route(&root, Behind::Site, "POST", "/pulp.css"),
            Reply::Status(405)
        );
        assert_eq!(
            route(&root, Behind::Site, "DELETE", "/"),
            Reply::Status(405)
        );
        assert_eq!(
            route(&root, Behind::Site, "GET", "/../Cargo.toml"),
            Reply::Status(404)
        );
    }

    #[test]
    fn test_route_behind_pulp_ui_serves_web_and_gate_and_forwards_the_rest() {
        let root = root();
        let pulp = |method, target| route(&root, Behind::PulpUi, method, target);
        let file = |rel| Reply::File(under(&root, rel));
        assert_eq!(pulp("GET", "/"), Reply::Forward);
        assert_eq!(pulp("GET", "/mill.js?v=1"), Reply::Forward);
        assert_eq!(pulp("POST", "/api/scan"), Reply::Forward);
        assert_eq!(pulp("GET", "/api/artifact/r1?format=xml"), Reply::Forward);
        assert_eq!(pulp("DELETE", "/api/pack"), Reply::Forward);
        assert_eq!(pulp("GET", "/webfonts/a.css"), Reply::Forward);
        assert_eq!(
            pulp("GET", "/web/test/harness.js"),
            file("web/test/harness.js")
        );
        assert_eq!(pulp("GET", "/__ui-test/hold"), Reply::Hold);
        assert_eq!(pulp("POST", "/__ui-test/done"), Reply::Done);
        assert_eq!(pulp("GET", "/__ui-test/env"), Reply::Env);
        assert_eq!(pulp("POST", "/__ui-test/restart"), Reply::Restart);
        assert_eq!(pulp("POST", "/__ui-test/hold-polls"), Reply::HoldPolls);
        assert_eq!(
            pulp("POST", "/__ui-test/release-polls"),
            Reply::ReleasePolls
        );
        assert_eq!(
            pulp("GET", "/__ui-test/restart"),
            file("site/__ui-test/restart")
        );
        assert_eq!(pulp("POST", "/web/mill.js"), Reply::Status(405));
        // The site server knows nothing of pulp ui.
        let site = |method, target| route(&root, Behind::Site, method, target);
        assert_eq!(site("GET", "/__ui-test/env"), file("site/__ui-test/env"));
        assert_eq!(site("POST", "/__ui-test/restart"), Reply::Status(405));
        assert_eq!(site("POST", "/api/scan"), Reply::Status(405));
    }

    #[test]
    fn test_read_head_with_headers_keeps_names_values_and_length() {
        let raw = "POST /api/pack HTTP/1.1\r\nHost: 127.0.0.1:1\r\nX-Pulp-Token:  abc \r\n\
                   content-length: 13\r\nbroken line\r\n\r\n{\"body\":true}";
        let mut reader = BufReader::new(raw.as_bytes());
        let head = read_head(&mut reader).unwrap();
        assert_eq!(
            (head.method.as_str(), head.target.as_str()),
            ("POST", "/api/pack")
        );
        assert_eq!(head.header("x-pulp-token"), Some("abc"));
        assert_eq!(head.header("HOST"), Some("127.0.0.1:1"));
        assert_eq!(head.content_length(), 13);
        assert_eq!(head.headers.len(), 3, "a line without a colon is dropped");
        let mut body = String::new();
        reader.read_to_string(&mut body).unwrap();
        assert_eq!(body, "{\"body\":true}", "the body is left unread");
    }

    #[test]
    fn test_page_server_with_meta_tags_picks_the_server() {
        let pulp =
            r#"<head><meta charset="utf-8"><meta name="ui-test-server" content="pulp-ui"></head>"#;
        assert_eq!(page_server(pulp).unwrap(), Behind::PulpUi);
        assert_eq!(
            page_server(r#"<meta name="viewport" content="width=device-width">"#).unwrap(),
            Behind::Site
        );
        assert_eq!(page_server("<html></html>").unwrap(), Behind::Site);
        let unknown = page_server(r#"<meta name="ui-test-server" content="nginx">"#);
        assert!(unknown.unwrap_err().to_string().contains("pulp-ui"));
    }

    #[test]
    fn test_test_pages_with_web_test_finds_each_page_and_its_server() {
        let pages = test_pages(&root()).unwrap();
        let find = |name: &str| {
            pages
                .iter()
                .find(|page| page.name == name)
                .map(|page| page.behind)
        };
        assert_eq!(find("mill.test.html"), Some(Behind::Site));
        assert_eq!(find("pages.test.html"), Some(Behind::Site));
        assert_eq!(find("local.test.html"), Some(Behind::PulpUi));
        let names: Vec<&str> = pages.iter().map(|page| page.name.as_str()).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted);
    }

    #[test]
    fn test_content_type_with_served_extensions_returns_mime_types() {
        let cases = [
            ("a.html", "text/html; charset=utf-8"),
            ("a.js", "text/javascript; charset=utf-8"),
            ("a.css", "text/css; charset=utf-8"),
            ("a.json", "application/json"),
            ("a.wasm", "application/wasm"),
            ("a.woff2", "font/woff2"),
            ("a.md", "text/markdown; charset=utf-8"),
            ("a.txt", "text/plain; charset=utf-8"),
            ("a.bin", "application/octet-stream"),
            ("LICENSE", "application/octet-stream"),
        ];
        for (file, mime) in cases {
            assert_eq!(content_type(Path::new(file)), mime, "{file}");
        }
    }

    fn get(server: &Server, request: &str) -> String {
        let mut stream = TcpStream::connect(server.addr).unwrap();
        stream.write_all(request.as_bytes()).unwrap();
        let mut reply = String::new();
        stream.read_to_string(&mut reply).unwrap();
        reply
    }

    #[test]
    fn test_server_with_real_requests_serves_files_and_refuses_traversal() {
        let server = Server::start(&root(), None, &Arc::default()).unwrap();
        let page = get(&server, "GET /web/mill.js HTTP/1.1\r\nHost: t\r\n\r\n");
        assert!(page.starts_with("HTTP/1.1 200 OK\r\n"), "{page:.80}");
        assert!(page.contains("Content-Type: text/javascript; charset=utf-8\r\n"));
        assert!(page.contains("export function mountMill"));

        let head = get(&server, "HEAD /pulp.css HTTP/1.1\r\n\r\n");
        assert!(head.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(head.ends_with("\r\n\r\n"), "HEAD has no body");

        for target in [
            "/../Cargo.toml",
            "/web/%2e%2e/Cargo.toml",
            "/no-such-file.txt",
        ] {
            let reply = get(&server, &format!("GET {target} HTTP/1.1\r\n\r\n"));
            assert!(
                reply.starts_with("HTTP/1.1 404 Not Found\r\n"),
                "{target}: {reply:.40}"
            );
            assert!(!reply.contains("[workspace]"), "{target} leaked a file");
        }
    }

    #[test]
    fn test_server_with_hold_waits_until_the_page_is_done() {
        let server = Server::start(&root(), None, &Arc::default()).unwrap();
        server.gate.set(false);
        let addr = server.addr;
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let mut stream = TcpStream::connect(addr).unwrap();
            stream
                .write_all(b"GET /__ui-test/hold HTTP/1.1\r\n\r\n")
                .unwrap();
            let mut reply = String::new();
            stream.read_to_string(&mut reply).unwrap();
            tx.send(reply).unwrap();
        });
        assert!(
            rx.recv_timeout(Duration::from_millis(200)).is_err(),
            "hold answered early"
        );

        let done = get(
            &server,
            "POST /__ui-test/done HTTP/1.1\r\nContent-Length: 2\r\n\r\n{}",
        );
        assert!(done.starts_with("HTTP/1.1 200 OK\r\n"));
        let held = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("hold released");
        assert!(held.starts_with("HTTP/1.1 200 OK\r\n"));
    }

    #[test]
    fn test_find_chrome_with_env_and_path_prefers_env_then_path_names() {
        let dir = private_scratch_dir().unwrap();
        let exe = |name: &str| dir.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
        std::fs::write(exe("chromium"), b"").unwrap();
        std::fs::write(exe("my-chrome"), b"").unwrap();
        let path = Some(dir.clone().into_os_string());

        let from_path = find_chrome(None, path.clone(), false).unwrap();
        let from_env = find_chrome(Some(exe("my-chrome").into()), path.clone(), false).unwrap();
        let by_name = find_chrome(Some("my-chrome".into()), path.clone(), false).unwrap();
        let missing = find_chrome(Some("/no/such/chrome".into()), path, false);
        let none = find_chrome(None, None, false);
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(from_path, exe("chromium"));
        assert_eq!(from_env, exe("my-chrome"));
        assert_eq!(by_name, exe("my-chrome"));
        assert!(missing.unwrap_err().to_string().contains("PULP_CHROME"));
        assert!(none.unwrap_err().to_string().contains("PULP_CHROME"));
    }

    #[test]
    fn test_private_scratch_dir_with_two_calls_makes_fresh_owner_only_folders() {
        let a = private_scratch_dir().unwrap();
        let b = private_scratch_dir().unwrap();
        assert_ne!(a, b);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&a).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o700);
        }
        std::fs::remove_dir(&a).unwrap();
        std::fs::remove_dir(&b).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn test_dump_dom_when_interrupted_stops_the_browser_promptly() {
        use std::os::unix::fs::PermissionsExt;
        let _serial = interrupt_lock();
        let dir = private_scratch_dir().unwrap();
        // A stand-in browser that never prints and ignores SIGINT, as headless Chrome does.
        let fake = dir.join("fake-chrome");
        std::fs::write(&fake, "#!/bin/sh\ntrap '' INT\nsleep 30\n").unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        INTERRUPTED.store(true, Ordering::SeqCst);
        let started = Instant::now();
        let result = dump_dom(
            &fake,
            "http://127.0.0.1:9/",
            &dir.join("profile"),
            Duration::from_secs(60),
        );
        INTERRUPTED.store(false, Ordering::SeqCst);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(result.unwrap_err().to_string().contains("interrupted"));
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "{:?}",
            started.elapsed()
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_stop_group_with_stubborn_helper_kills_the_whole_group() {
        use std::os::unix::fs::PermissionsExt;
        use std::os::unix::process::CommandExt;
        let dir = private_scratch_dir().unwrap();
        // A browser that ignores SIGTERM, with a helper that does too.
        let browser = dir.join("browser");
        let helper_pid = dir.join("helper.pid");
        let script = format!(
            "#!/bin/sh\ntrap '' TERM\nsleep 30 &\necho $! > '{}'\nexec sleep 30\n",
            helper_pid.display()
        );
        std::fs::write(&browser, script).unwrap();
        std::fs::set_permissions(&browser, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut child = Command::new(&browser).process_group(0).spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let helper = loop {
            let pid = std::fs::read_to_string(&helper_pid).unwrap_or_default();
            if let Ok(pid) = pid.trim().parse::<libc::pid_t>() {
                break pid;
            }
            assert!(Instant::now() < deadline, "the helper never started");
            thread::sleep(Duration::from_millis(20));
        };

        let started = Instant::now();
        stop_group(&mut child, Duration::from_millis(300));
        let stopped = started.elapsed();
        // The orphaned helper is reaped by init soon after it dies.
        let gone = (0..100).any(|_| {
            thread::sleep(Duration::from_millis(20));
            // SAFETY: kill(2) with signal 0 only checks that the process exists.
            let alive = unsafe { libc::kill(helper, 0) } == 0;
            !alive
        });
        let _ = std::fs::remove_dir_all(&dir);

        assert!(gone, "the helper outlived the stop");
        assert!(stopped < Duration::from_secs(5), "{stopped:?}");
    }

    #[test]
    fn test_read_dump_with_trailing_output_stops_at_the_document_end() {
        let dom = read_dump(&b"<html><body>x</body></html>\n"[..]);
        assert_eq!(dom, "<html><body>x</body></html>\n");
        let partial = read_dump(&b"<html><body>cut"[..]);
        assert_eq!(partial, "<html><body>cut");
    }

    const PASSING: &str = r#"<!DOCTYPE html><html><head></head><body>
<pre id="results" data-status="pass">PASS mount: empty state
PASS pulp: shows &lt;documents&gt; &amp; Ready
# 2 passed, 0 failed in 40 ms
</pre><iframe src="/__ui-test/hold" hidden=""></iframe></body></html>
"#;

    #[test]
    fn test_parse_results_with_passing_block_returns_tests_and_notes() {
        let results = parse_results(PASSING).unwrap();
        assert_eq!(results.status, "pass");
        assert_eq!(results.passed(), 2);
        assert_eq!(results.failed(), 0);
        assert_eq!(results.tests[1].name, "pulp: shows <documents> & Ready");
        assert_eq!(results.notes, vec!["# 2 passed, 0 failed in 40 ms"]);
        assert!(results.is_pass());
    }

    #[test]
    fn test_parse_results_with_failure_keeps_its_detail_lines() {
        let dom = r#"<html><body><pre class="log">PASS decoy</pre>
<pre data-status="fail" id="results">PASS a
FAIL b: issues
  expected "combined", got "preview"
  at mill.test.js:10
PASS c
</pre></body></html>"#;
        let results = parse_results(dom).unwrap();
        assert_eq!(results.status, "fail");
        assert_eq!(results.passed(), 2);
        assert_eq!(results.failed(), 1);
        assert_eq!(
            results.tests[1],
            TestLine {
                passed: false,
                name: "b: issues".into(),
                detail: vec![
                    "expected \"combined\", got \"preview\"".into(),
                    "at mill.test.js:10".into()
                ],
            }
        );
        assert!(!results.is_pass());
    }

    #[test]
    fn test_parse_results_with_unfinished_or_empty_page_is_not_a_pass() {
        let running = r#"<html><pre id="results" data-status="running">PASS a
</pre></html>"#;
        let running = parse_results(running).unwrap();
        assert_eq!(running.status, "running");
        assert!(!running.is_pass());

        let empty = parse_results(r#"<pre id="results" data-status="pass"></pre>"#).unwrap();
        assert!(!empty.is_pass(), "a page with no tests does not pass");
    }

    #[test]
    fn test_page_problem_with_unfinished_or_empty_pages_names_the_problem() {
        let page = |status: &str, tests: &[bool]| Results {
            status: status.into(),
            tests: tests
                .iter()
                .map(|&passed| TestLine::new(passed, "t"))
                .collect(),
            notes: Vec::new(),
        };
        assert_eq!(page_problem(&page("pass", &[true])), None);
        assert_eq!(page_problem(&page("fail", &[true, false])), None);
        for unfinished in [
            page("running", &[true]),
            page("running", &[false]),
            page("", &[]),
        ] {
            let problem = page_problem(&unfinished).unwrap();
            assert!(problem.contains("still"), "{problem}");
            assert!(problem.contains(HOLD_PATH), "{problem}");
        }
        assert_eq!(
            page_problem(&page("pass", &[])).as_deref(),
            Some("the page ran no tests")
        );
        let problem = page_problem(&page("fail", &[true])).unwrap();
        assert!(problem.contains("without a failing test"), "{problem}");
    }

    #[test]
    fn test_parse_results_without_block_returns_none() {
        assert_eq!(
            parse_results("<html><body><p>error</p></body></html>"),
            None
        );
        assert_eq!(
            parse_results(r#"<pre data-id="results">PASS a</pre>"#),
            None
        );
        assert_eq!(
            parse_results(r#"<pre id="results" data-status="pass">PASS a"#),
            None
        );
        assert_eq!(parse_results(""), None);
    }

    #[test]
    fn test_unescape_with_entities_restores_text_once() {
        assert_eq!(
            unescape("a &lt;b&gt; &amp;lt; &quot;c&quot; &#39;d&#39;"),
            "a <b> &lt; \"c\" 'd'"
        );
        assert_eq!(unescape("fish & chips &unknown;"), "fish & chips &unknown;");
    }
}
