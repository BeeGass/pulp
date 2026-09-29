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

/// Test pages live here, relative to the workspace root.
const TEST_DIR: &str = "web/test";
/// Longest a page may run before the server lets its load event fire.
const PAGE_BUDGET: Duration = Duration::from_secs(120);
/// Time on top of the budget for Chrome to start and print the page.
const CHROME_GRACE: Duration = Duration::from_secs(30);
const HOLD_PATH: &str = "/__ui-test/hold";
const DONE_PATH: &str = "/__ui-test/done";
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

/// Set by Ctrl-C or SIGTERM during a run, so Chrome and its profile are cleaned
/// up instead of left running: headless Chrome does not exit on SIGINT.
static INTERRUPTED: AtomicBool = AtomicBool::new(false);

/// Run every test page, or with `serve_only` keep serving for a browser.
pub fn run(root: &Path, serve_only: bool) -> anyhow::Result<()> {
    let pages = test_pages(root)?;
    let server = Server::start(root)?;
    if serve_only {
        serve_forever(&server, &pages);
    }
    let chrome = find_chrome(
        std::env::var_os("PULP_CHROME"),
        std::env::var_os("PATH"),
        cfg!(target_os = "macos"),
    )?;
    eprintln!("xtask ui-test: {} on {}", chrome.display(), server.url("/"));
    catch_interrupts();

    let started = Instant::now();
    let scratch = private_scratch_dir()?;
    let (mut passed, mut failed, mut broken) = (0, 0, 0);
    for (index, page) in pages.iter().enumerate() {
        println!("{TEST_DIR}/{page}");
        server.gate.set(false);
        let url = server.url(&format!("/{TEST_DIR}/{page}"));
        let dom = dump_dom(
            &chrome,
            &url,
            &scratch.join(index.to_string()),
            PAGE_BUDGET + CHROME_GRACE,
        );
        if INTERRUPTED.load(Ordering::SeqCst) {
            remove_dir_eventually(&scratch);
            bail!("ui-test interrupted; Chrome stopped and its profile removed");
        }
        let results = match dom {
            Ok(dom) => parse_results(&dom),
            Err(err) => {
                broken += 1;
                println!("  FAIL {err:#}");
                continue;
            }
        };
        let Some(results) = results else {
            broken += 1;
            println!("  FAIL no results block; the test page failed before it could report");
            continue;
        };
        print_results(&results);
        passed += results.passed();
        failed += results.failed();
        if let Some(problem) = page_problem(&results) {
            broken += 1;
            println!("  FAIL {problem}");
        }
    }
    remove_dir_eventually(&scratch);

    let secs = started.elapsed().as_secs_f64();
    eprintln!("xtask ui-test: {passed} passed, {failed} failed in {secs:.1}s");
    if failed > 0 || broken > 0 {
        bail!("ui-test failed: {failed} failing test(s), {broken} page(s) without a clean result");
    }
    Ok(())
}

/// Turn Ctrl-C and SIGTERM into a flag the run loop checks, so it can stop
/// Chrome and remove the profile before exiting. Without a handler the process
/// would die at once and leave Chrome running.
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

fn serve_forever(server: &Server, pages: &[String]) -> ! {
    // Nobody reads the dump here, so pages load without waiting on the gate.
    server.gate.set(true);
    eprintln!(
        "xtask ui-test: serving web/ and site/ at {}",
        server.url("/")
    );
    for page in pages {
        eprintln!("  {}", server.url(&format!("/{TEST_DIR}/{page}")));
    }
    eprintln!("  {}  landing page", server.url("/"));
    eprintln!("  {}  browser mill", server.url("/mill/"));
    eprintln!("Ctrl-C stops the server.");
    loop {
        thread::park();
    }
}

/// `*.test.html` file names under `web/test/`, sorted.
fn test_pages(root: &Path) -> anyhow::Result<Vec<String>> {
    let dir = root.join(TEST_DIR);
    let entries = std::fs::read_dir(&dir).with_context(|| format!("read {}", dir.display()))?;
    let mut pages: Vec<String> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| name.ends_with(".test.html"))
        .collect();
    pages.sort();
    if pages.is_empty() {
        bail!("no *.test.html pages in {}", dir.display());
    }
    Ok(pages)
}

/* ---------- static server ---------- */

/// Serves `web/` and `site/` on an ephemeral 127.0.0.1 port until the process exits.
struct Server {
    addr: SocketAddr,
    gate: Arc<Gate>,
}

impl Server {
    fn start(root: &Path) -> anyhow::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").context("bind the ui-test server")?;
        let addr = listener.local_addr()?;
        let gate = Arc::new(Gate::default());
        let root = root.to_path_buf();
        let shared = Arc::clone(&gate);
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let root = root.clone();
                let gate = Arc::clone(&shared);
                thread::spawn(move || {
                    // A browser that drops a connection early is not a test failure.
                    let _ = handle(stream, &root, &gate);
                });
            }
        });
        Ok(Self { addr, gate })
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
    Status(u16),
}

fn route(root: &Path, method: &str, target: &str) -> Reply {
    let path = target.split(['?', '#']).next().unwrap_or_default();
    match (method, path) {
        ("GET", HOLD_PATH) => Reply::Hold,
        ("POST", DONE_PATH) => Reply::Done,
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

fn handle(stream: TcpStream, root: &Path, gate: &Gate) -> io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let (method, target, body_len) = {
        let mut head = (&mut reader).take(MAX_HEAD);
        let mut line = String::new();
        head.read_line(&mut line)?;
        let mut parts = line.split_whitespace();
        let method = parts.next().unwrap_or_default().to_string();
        let target = parts.next().unwrap_or_default().to_string();
        (method, target, read_headers(&mut head)?)
    };
    // Read any body so closing the socket does not reset the browser's request.
    io::copy(&mut (&mut reader).take(body_len), &mut io::sink())?;

    let head_only = method == "HEAD";
    match route(root, &method, &target) {
        Reply::File(file) => match std::fs::read(&file) {
            Ok(body) => respond(stream, 200, content_type(&file), &body, head_only),
            Err(_) => respond(stream, 404, "text/plain", b"not found\n", head_only),
        },
        Reply::Hold => {
            gate.wait(PAGE_BUDGET);
            respond(stream, 200, "text/html; charset=utf-8", b"", false)
        }
        Reply::Done => {
            gate.set(true);
            respond(stream, 200, "text/plain", b"done\n", false)
        }
        Reply::Status(code) => {
            let body: &[u8] = if code == 404 {
                b"not found\n"
            } else {
                b"method not allowed\n"
            };
            respond(stream, code, "text/plain", body, head_only)
        }
    }
}

/// Consume header lines and return the request's `Content-Length`.
fn read_headers(head: &mut impl BufRead) -> io::Result<u64> {
    let mut body_len = 0;
    loop {
        let mut line = String::new();
        if head.read_line(&mut line)? == 0 {
            return Ok(body_len);
        }
        let line = line.trim_end();
        if line.is_empty() {
            return Ok(body_len);
        }
        let length = line
            .split_once(':')
            .filter(|(name, _)| name.trim().eq_ignore_ascii_case("content-length"));
        if let Some((_, value)) = length {
            body_len = value.trim().parse().unwrap_or(0);
        }
    }
}

fn respond(
    mut stream: TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
    head_only: bool,
) -> io::Result<()> {
    let reason = match status {
        200 => "OK",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Error",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\
         Cache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\n",
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
    let mut child = Command::new(chrome)
        .args(chrome_args(url, profile))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
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
        if INTERRUPTED.load(Ordering::SeqCst) {
            stop(&mut child);
            bail!("interrupted");
        }
        match dom_rx.recv_timeout(Duration::from_millis(100)) {
            Err(RecvTimeoutError::Timeout) if Instant::now() < deadline => {}
            other => break other,
        }
    };
    stop(&mut child);
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

/// Stop Chrome. SIGTERM makes it close its helper processes too; a kill leaves
/// them running for several seconds, so it is only the fallback.
fn stop(child: &mut Child) {
    #[cfg(unix)]
    {
        let asked = Command::new("kill")
            .args(["-TERM", &child.id().to_string()])
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success());
        let deadline = Instant::now() + Duration::from_secs(3);
        while asked && Instant::now() < deadline {
            if let Ok(Some(_)) = child.try_wait() {
                return;
            }
            thread::sleep(Duration::from_millis(50));
        }
    }
    let _ = child.kill();
    let _ = child.wait();
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
        assert_eq!(route(&root, "GET", "/"), file("site/index.html"));
        assert_eq!(route(&root, "GET", "/mill"), file("site/mill/index.html"));
        assert_eq!(route(&root, "GET", "/mill/"), file("site/mill/index.html"));
        assert_eq!(route(&root, "HEAD", "/web/mill.js"), file("web/mill.js"));
    }

    #[test]
    fn test_route_with_gate_and_bad_requests_returns_matching_replies() {
        let root = root();
        assert_eq!(route(&root, "GET", "/__ui-test/hold"), Reply::Hold);
        assert_eq!(route(&root, "POST", "/__ui-test/done"), Reply::Done);
        assert_eq!(route(&root, "POST", "/pulp.css"), Reply::Status(405));
        assert_eq!(route(&root, "DELETE", "/"), Reply::Status(405));
        assert_eq!(route(&root, "GET", "/../Cargo.toml"), Reply::Status(404));
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
        let server = Server::start(&root()).unwrap();
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
        let server = Server::start(&root()).unwrap();
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
