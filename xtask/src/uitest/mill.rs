//! The mill origin: a real `pulp ui` behind the ui-test server.
//!
//! A test page that declares `<meta name="ui-test-server" content="pulp-ui">`
//! is served from a second server, the mill origin. It answers `/web/` and
//! `/__ui-test/` itself and forwards every other request to a `pulp ui` built
//! from this checkout. The page opens the shell in a window of its own, from
//! the link `pulp ui` printed, and drives it as a person would, so
//! `web/index.html`'s token header, its busy and stale-session answers,
//! progress polling, and full-dump fetch all run against the real server, with
//! every header pulp sends left as it is.
//!
//! The forward rewrites `Host`, and an `Origin` naming the mill origin, to the
//! `pulp ui` address, which is what pulp's same-origin checks expect. Any other
//! `Origin` passes through unchanged, so pulp still turns other sites away.
//!
//! The mill origin's own responses carry the `Cross-Origin-Opener-Policy` that
//! pulp sends: a window keeps its handle on a window it opens only when their
//! origins and opener policies match.
//!
//! Besides the gate, the mill origin answers these harness paths:
//!
//! - `GET /__ui-test/env`: the folders a page scans and the path of the
//!   session link, as JSON (see [`Fixtures`]).
//! - `POST /__ui-test/restart`: stop `pulp ui` and start a fresh one, with a
//!   new session token, so an open page holds a stale one.
//! - `POST /__ui-test/hold-polls`: hold the shell's `/api/progress` polls and
//!   answer once one is held, so a page can end a pack with a poll still out;
//!   `POST /__ui-test/release-polls` lets them through.
//!
//! `pulp ui` runs with `TMPDIR` inside the run's scratch folder, so the sample
//! it writes and its extractors' temp files go when the scratch folder does,
//! and with one extraction thread (`RAYON_NUM_THREADS=1`), so a folder of heavy
//! files packs one file at a time: slowly enough on any machine for a page to
//! watch its progress and to find the mill busy.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, anyhow, bail};

use super::{Head, PAGE_BUDGET, interrupted, respond, stop};
use crate::host::Host;

/// Longest `pulp ui` may take to report its address after it is spawned.
const START_TIMEOUT: Duration = Duration::from_secs(30);
/// How long after printing its bare address `pulp ui` gets to print the link
/// that carries the session token, before the bare address is taken as the link.
const LINK_GRACE: Duration = Duration::from_secs(1);
/// Ports tried before giving up; another process can take a free port first.
const START_TRIES: usize = 4;
/// Time `pulp ui` gets to finish open requests after SIGTERM; its own grace is 5s.
const STOP_GRACE: Duration = Duration::from_secs(8);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Heavy files in the slow folder. Each is extracted in a child process, one
/// at a time, so the pack lasts seconds.
const SLOW_FILES: usize = 150;
/// Parts of the big folder; their dump overflows the mill's 32 KiB preview.
const BIG_PARTS: usize = 3;
const BIG_LINES: usize = 300;
/// The last line of the big folder, well past the preview.
const BIG_END: &str = "the end of the big folder";
/// How much of `pulp ui`'s stderr is kept for failure reports.
const LOG_KEEP: usize = 16 * 1024;

/// A running `pulp ui`, and what the mill origin tells test pages about it.
pub(super) struct Mill {
    pulp: Mutex<PulpUi>,
    log: Arc<Log>,
    /// The fixtures as JSON object members, for [`Mill::env`].
    fixtures: String,
    /// The `Cross-Origin-Opener-Policy` pulp sends, if any.
    opener_policy: Option<String>,
    polls: PollHold,
}

impl Mill {
    /// Build pulp, write the fixtures and pulp's temp folder under `scratch`,
    /// and start `pulp ui`. The returned guard stops it when dropped.
    pub(super) fn start(root: &Path, host: &Host, scratch: &Path) -> anyhow::Result<Running> {
        let exe = build(host)?;
        let fixtures = Fixtures::write(root, &scratch.join("fixtures"))
            .context("write the mill test folders")?;
        let tmp = scratch.join("pulp-tmp");
        std::fs::create_dir(&tmp).with_context(|| format!("create {}", tmp.display()))?;
        let log = Arc::new(Log::default());
        // From here on, dropping `pulp` on an error stops it.
        let pulp = PulpUi::start(exe, tmp, Arc::clone(&log))?;
        let opener_policy = opener_policy(pulp.upstream()).context("ask pulp ui for its page")?;
        Ok(Running(Arc::new(Self {
            pulp: Mutex::new(pulp),
            log,
            fixtures: fixtures.json_members(),
            opener_policy,
            polls: PollHold::default(),
        })))
    }

    fn pulp(&self) -> MutexGuard<'_, PulpUi> {
        self.pulp.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Where `pulp ui` listens now.
    pub(super) fn upstream(&self) -> SocketAddr {
        self.pulp().upstream()
    }

    /// Stop `pulp ui` and start a fresh one with a new session token.
    pub(super) fn restart(&self) -> anyhow::Result<()> {
        let mut pulp = self.pulp();
        pulp.stop();
        pulp.launch()
    }

    pub(super) fn stop(&self) {
        self.pulp().stop();
    }

    /// The fixtures and the session link's path (`/?token=…`), as JSON, for
    /// `GET /__ui-test/env`. The link changes when pulp ui restarts.
    pub(super) fn env(&self) -> String {
        let page = json_quote(&self.page());
        format!("{{{},\"page\":{page}}}", self.fixtures)
    }

    /// The path of the link pulp printed: `/?token=…`, or `/`.
    pub(super) fn page(&self) -> String {
        self.pulp().page.clone()
    }

    /// The opener policy the mill origin's own pages carry, to match pulp's.
    pub(super) fn opener_policy(&self) -> Option<&str> {
        self.opener_policy.as_deref()
    }

    /// The end of `pulp ui`'s stderr, across restarts.
    pub(super) fn log(&self) -> String {
        self.log.text()
    }

    /// Forward one request to `pulp ui`; a progress poll waits while polls are held.
    pub(super) fn forward(
        &self,
        browser: TcpStream,
        head: &Head,
        body: &[u8],
        own: SocketAddr,
    ) -> io::Result<()> {
        if head.target.split('?').next() == Some("/api/progress") {
            self.polls.pass();
        }
        forward(browser, head, body, self.upstream(), own)
    }

    /// Hold progress polls from now on. True once one is held; false, and no
    /// longer holding, when none comes within `budget`.
    pub(super) fn hold_polls(&self, budget: Duration) -> bool {
        self.polls.hold(budget)
    }

    /// Let held progress polls through and stop holding new ones.
    pub(super) fn release_polls(&self) {
        self.polls.release();
    }
}

/// Holds progress polls on a test's request.
#[derive(Default)]
struct PollHold {
    state: Mutex<Polls>,
    changed: Condvar,
}

#[derive(Default)]
struct Polls {
    holding: bool,
    /// Polls waiting in [`PollHold::pass`].
    held: usize,
}

impl PollHold {
    fn lock(&self) -> MutexGuard<'_, Polls> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn hold(&self, budget: Duration) -> bool {
        let mut polls = self.lock();
        polls.holding = true;
        let (mut polls, _) = self
            .changed
            .wait_timeout_while(polls, budget, |polls| polls.held == 0)
            .unwrap_or_else(PoisonError::into_inner);
        if polls.held == 0 {
            polls.holding = false;
        }
        polls.held > 0
    }

    fn release(&self) {
        self.lock().holding = false;
        self.changed.notify_all();
    }

    /// Wait while polls are held, for at most the page budget.
    fn pass(&self) {
        let mut polls = self.lock();
        if !polls.holding {
            return;
        }
        polls.held += 1;
        self.changed.notify_all();
        let (mut polls, _) = self
            .changed
            .wait_timeout_while(polls, PAGE_BUDGET, |polls| polls.holding)
            .unwrap_or_else(PoisonError::into_inner);
        polls.held -= 1;
    }
}

/// Stops `pulp ui` when dropped, however the run ends. Server threads share the
/// [`Mill`], so it is never dropped itself before the process exits.
pub(super) struct Running(Arc<Mill>);

impl Running {
    pub(super) fn mill(&self) -> &Arc<Mill> {
        &self.0
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        self.0.stop();
    }
}

/* ---------- forwarding ---------- */

/// Send one request to `pulp ui` and copy its answer back to the browser.
fn forward(
    mut browser: TcpStream,
    head: &Head,
    body: &[u8],
    upstream: SocketAddr,
    own: SocketAddr,
) -> io::Result<()> {
    let Ok(mut pulp) = TcpStream::connect_timeout(&upstream, CONNECT_TIMEOUT) else {
        return respond(
            browser,
            502,
            "text/plain",
            b"pulp ui is not answering\n",
            false,
        );
    };
    pulp.set_read_timeout(Some(PAGE_BUDGET))?;
    pulp.write_all(forwarded_head(head, upstream, own).as_bytes())?;
    pulp.write_all(body)?;
    pulp.flush()?;
    // The request asked pulp to close after one answer, so this ends with it.
    io::copy(&mut pulp, &mut browser)?;
    browser.flush()
}

/// The request head as `pulp ui` must see it: addressed to pulp, from pulp's
/// own origin when the browser named the mill origin, closed after one answer.
fn forwarded_head(head: &Head, upstream: SocketAddr, own: SocketAddr) -> String {
    const HOP_BY_HOP: &[&str] = &[
        "host",
        "connection",
        "keep-alive",
        "proxy-connection",
        "te",
        "upgrade",
    ];
    let own_origin = format!("http://{own}");
    let mut out = format!(
        "{} {} HTTP/1.1\r\nHost: {upstream}\r\n",
        head.method, head.target
    );
    for (name, value) in &head.headers {
        if HOP_BY_HOP.iter().any(|hop| name.eq_ignore_ascii_case(hop)) {
            continue;
        }
        if name.eq_ignore_ascii_case("origin") && *value == own_origin {
            out.push_str(&format!("{name}: http://{upstream}\r\n"));
        } else {
            out.push_str(&format!("{name}: {value}\r\n"));
        }
    }
    out.push_str("Connection: close\r\n\r\n");
    out
}

/* ---------- pulp ui ---------- */

/// `pulp ui --no-open` on a free port. Dropping it stops the process.
struct PulpUi {
    exe: PathBuf,
    tmp: PathBuf,
    child: Option<Child>,
    port: u16,
    /// Path and query of the link pulp printed: `/?token=…`, or `/`.
    page: String,
    log: Arc<Log>,
}

impl PulpUi {
    fn start(exe: PathBuf, tmp: PathBuf, log: Arc<Log>) -> anyhow::Result<Self> {
        let mut pulp = Self {
            exe,
            tmp,
            child: None,
            port: 0,
            page: String::new(),
            log,
        };
        pulp.launch()?;
        Ok(pulp)
    }

    fn upstream(&self) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], self.port))
    }

    /// Start on a free port. The port is only known free until it is released
    /// for pulp to bind, so a start that fails is tried again on another.
    fn launch(&mut self) -> anyhow::Result<()> {
        let mut failure = anyhow!("pulp ui was not started");
        for _ in 0..START_TRIES {
            if interrupted() {
                bail!("ui-test interrupted while starting pulp ui");
            }
            let port = free_port().context("find a free port for pulp ui")?;
            match self.spawn(port) {
                Ok((child, page)) => {
                    self.child = Some(child);
                    self.port = port;
                    self.page = page;
                    return Ok(());
                }
                Err(err) => failure = err,
            }
        }
        Err(failure)
    }

    /// Spawn `pulp ui` on `port` and wait for the link it prints once it
    /// serves there: the one that carries a query (the session token) if it
    /// prints one, else its bare address. Returns the process and the link's
    /// path.
    fn spawn(&self, port: u16) -> anyhow::Result<(Child, String)> {
        let mut child = Command::new(&self.exe)
            .args(["ui", "--no-open", "--port", &port.to_string()])
            .env("TMPDIR", &self.tmp)
            .env("RAYON_NUM_THREADS", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("start {}", self.exe.display()))?;
        let stderr = child.stderr.take().context("capture pulp ui's stderr")?;
        let (lines_tx, lines) = mpsc::channel();
        let log = Arc::clone(&self.log);
        thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                log.push(&line);
                // Only the start listens; later lines just go to the log.
                let _ = lines_tx.send(line);
            }
        });

        let origin = format!("http://127.0.0.1:{port}");
        let deadline = Instant::now() + START_TIMEOUT;
        // When pulp printed its bare address; the link with the token follows.
        let mut bare_since: Option<Instant> = None;
        loop {
            match lines.recv_timeout(Duration::from_millis(50)) {
                Ok(line) => match link_path(&line, &origin) {
                    Some(page) if page != "/" => return Ok((child, page)),
                    Some(_) => {
                        bare_since.get_or_insert_with(Instant::now);
                    }
                    None => {}
                },
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    // stderr closed: pulp exited, most likely because the port was taken.
                    stop(&mut child, STOP_GRACE);
                    bail!(
                        "pulp ui stopped before serving {origin}; its log ends with:\n{}",
                        self.log.text()
                    );
                }
            }
            if bare_since.is_some_and(|since| since.elapsed() >= LINK_GRACE) {
                return Ok((child, "/".into()));
            }
            if Instant::now() >= deadline || interrupted() {
                stop(&mut child, STOP_GRACE);
                bail!(
                    "pulp ui did not print a link to {origin} within {}s; its log ends with:\n{}",
                    START_TIMEOUT.as_secs(),
                    self.log.text()
                );
            }
        }
    }

    fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            stop(&mut child, STOP_GRACE);
        }
    }
}

impl Drop for PulpUi {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The path of a link to `origin` in a line pulp printed: `/` for the bare
/// origin, else the path and query after it (`/?token=…`).
fn link_path(line: &str, origin: &str) -> Option<String> {
    line.split_whitespace()
        .find_map(|word| match word.strip_prefix(origin)? {
            "" => Some("/".to_string()),
            rest if rest.starts_with('/') => Some(rest.to_string()),
            // Another port that starts with the same digits.
            _ => None,
        })
}

/// The `Cross-Origin-Opener-Policy` of the page pulp ui serves at `/`.
fn opener_policy(upstream: SocketAddr) -> io::Result<Option<String>> {
    let mut pulp = TcpStream::connect_timeout(&upstream, CONNECT_TIMEOUT)?;
    pulp.set_read_timeout(Some(CONNECT_TIMEOUT))?;
    write!(
        pulp,
        "GET / HTTP/1.1\r\nHost: {upstream}\r\nConnection: close\r\n\r\n"
    )?;
    // A response head reads like a request head: a first line, then headers.
    let answer = super::read_head(&mut BufReader::new(pulp))?;
    if answer.method != "HTTP/1.1" && answer.method != "HTTP/1.0" {
        return Err(io::Error::other(format!("not an HTTP answer: {answer:?}")));
    }
    Ok(answer
        .header("cross-origin-opener-policy")
        .map(str::to_string))
}

fn free_port() -> io::Result<u16> {
    Ok(TcpListener::bind("127.0.0.1:0")?.local_addr()?.port())
}

/// The last [`LOG_KEEP`] bytes of `pulp ui`'s stderr.
#[derive(Default)]
struct Log(Mutex<String>);

impl Log {
    fn push(&self, line: &str) {
        let mut text = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        text.push_str(line);
        text.push('\n');
        if text.len() > LOG_KEEP {
            let mut cut = text.len() - LOG_KEEP;
            while !text.is_char_boundary(cut) {
                cut += 1;
            }
            text.drain(..cut);
        }
    }

    fn text(&self) -> String {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/* ---------- build ---------- */

/// `cargo build` the pulp binary and return its path.
fn build(host: &Host) -> anyhow::Result<PathBuf> {
    let args: Vec<String> = [
        "build",
        "--package",
        "pulp",
        "--bin",
        "pulp",
        "--message-format=json-render-diagnostics",
    ]
    .map(String::from)
    .into();
    eprintln!("xtask ui-test: building pulp for the local mill tests");
    let mut child = crate::cargo::command(host, &args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .context("spawn cargo build")?;
    let mut stdout = child.stdout.take().context("capture cargo's stdout")?;
    let (messages_tx, messages) = mpsc::channel();
    thread::spawn(move || {
        let mut text = String::new();
        let _ = stdout.read_to_string(&mut text);
        let _ = messages_tx.send(text);
    });
    let status = loop {
        if interrupted() {
            stop(&mut child, Duration::from_secs(3));
            bail!("ui-test interrupted while building pulp");
        }
        if let Some(status) = child.try_wait().context("wait for cargo build")? {
            break status;
        }
        thread::sleep(Duration::from_millis(100));
    };
    if !status.success() {
        bail!("cargo {} failed", args.join(" "));
    }
    let messages = messages.recv().unwrap_or_default();
    executable_from_messages(&messages)
        .context("cargo build did not report where it put the pulp binary")
}

/// The `pulp` executable among cargo's JSON messages.
fn executable_from_messages(messages: &str) -> Option<PathBuf> {
    messages
        .lines()
        .filter(|line| line.contains(r#""reason":"compiler-artifact""#))
        .filter_map(|line| json_string_field(line, "executable"))
        .map(PathBuf::from)
        .rfind(|path| path.file_stem().is_some_and(|stem| stem == "pulp"))
}

/// The string value of `"key": "…"` in one line of JSON, unescaped. `None` when
/// the key is missing, its value is not a string, or an escape is malformed.
fn json_string_field(line: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\":");
    let at = line.find(&needle)? + needle.len();
    let mut chars = line[at..].trim_start().strip_prefix('"')?.chars();
    let mut out = String::new();
    loop {
        match chars.next()? {
            '"' => return Some(out),
            '\\' => match chars.next()? {
                '"' => out.push('"'),
                '\\' => out.push('\\'),
                '/' => out.push('/'),
                'b' => out.push('\u{8}'),
                'f' => out.push('\u{c}'),
                'n' => out.push('\n'),
                'r' => out.push('\r'),
                't' => out.push('\t'),
                'u' => {
                    let unit = hex4(&mut chars)?;
                    let code = if (0xd800..0xdc00).contains(&unit) {
                        // A high surrogate pairs with the escape that follows it.
                        if chars.next()? != '\\' || chars.next()? != 'u' {
                            return None;
                        }
                        let low = hex4(&mut chars)?;
                        if !(0xdc00..0xe000).contains(&low) {
                            return None;
                        }
                        0x10000 + ((unit - 0xd800) << 10) + (low - 0xdc00)
                    } else {
                        unit
                    };
                    out.push(char::from_u32(code)?);
                }
                _ => return None,
            },
            c => out.push(c),
        }
    }
}

fn hex4(chars: &mut std::str::Chars<'_>) -> Option<u32> {
    let mut value = 0;
    for _ in 0..4 {
        value = value * 16 + chars.next()?.to_digit(16)?;
    }
    Some(value)
}

/// `text` as a JSON string literal.
fn json_quote(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if u32::from(c) < 0x20 => out.push_str(&format!("\\u{:04x}", u32::from(c))),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/* ---------- fixtures ---------- */

/// Folders the mill page scans: the repository's `testdata/`, a big folder
/// whose dump overflows the preview (so Copy fetches the whole dump), and a
/// slow folder of heavy files.
struct Fixtures {
    testdata: PathBuf,
    big: PathBuf,
    slow: PathBuf,
}

impl Fixtures {
    fn write(root: &Path, dir: &Path) -> io::Result<Self> {
        let big = dir.join("big");
        std::fs::create_dir_all(&big)?;
        for part in 1..=BIG_PARTS {
            let mut text = String::new();
            for line in 1..=BIG_LINES {
                text.push_str(&format!(
                    "part {part} line {line:03}: the mill keeps every line of a big folder\n"
                ));
            }
            if part == BIG_PARTS {
                text.push_str(BIG_END);
                text.push('\n');
            }
            std::fs::write(big.join(format!("part-{part}.txt")), text)?;
        }
        let slow = dir.join("slow");
        std::fs::create_dir_all(&slow)?;
        for note in 0..SLOW_FILES {
            let rtf = format!("{{\\rtf1\\ansi note {note} of the slow folder\\par}}");
            std::fs::write(slow.join(format!("note-{note:03}.rtf")), rtf)?;
        }
        Ok(Self {
            testdata: root.join("testdata"),
            big,
            slow,
        })
    }

    /// The folders as JSON object members, without the braces.
    fn json_members(&self) -> String {
        let path = |path: &Path| json_quote(&path.to_string_lossy());
        format!(
            "\"testdata\":{},\"big\":{},\"bigEnd\":{},\"slow\":{},\"slowFiles\":{SLOW_FILES}",
            path(&self.testdata),
            path(&self.big),
            json_quote(BIG_END),
            path(&self.slow),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::uitest::{Server, private_scratch_dir};

    fn head(method: &str, target: &str, headers: &[(&str, &str)]) -> Head {
        Head {
            method: method.into(),
            target: target.into(),
            headers: headers
                .iter()
                .map(|&(name, value)| (name.into(), value.into()))
                .collect(),
        }
    }

    fn addr(port: u16) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], port))
    }

    #[test]
    fn test_forwarded_head_with_mill_origin_speaks_as_pulp() {
        let request = head(
            "POST",
            "/api/scan?x=1",
            &[
                ("Host", "127.0.0.1:4000"),
                ("Connection", "keep-alive"),
                ("Content-Type", "application/json"),
                ("x-pulp-token", "abc"),
                ("Origin", "http://127.0.0.1:4000"),
                ("Content-Length", "2"),
            ],
        );
        let sent = forwarded_head(&request, addr(5000), addr(4000));
        assert_eq!(
            sent,
            "POST /api/scan?x=1 HTTP/1.1\r\nHost: 127.0.0.1:5000\r\n\
             Content-Type: application/json\r\nx-pulp-token: abc\r\n\
             Origin: http://127.0.0.1:5000\r\nContent-Length: 2\r\nConnection: close\r\n\r\n"
        );
    }

    #[test]
    fn test_forwarded_head_with_other_origin_keeps_it_for_pulp_to_refuse() {
        let request = head(
            "POST",
            "/api/pack",
            &[
                ("origin", "http://127.0.0.1:4001"),
                ("Keep-Alive", "timeout=5"),
            ],
        );
        let sent = forwarded_head(&request, addr(5000), addr(4000));
        assert!(
            sent.contains("\r\norigin: http://127.0.0.1:4001\r\n"),
            "{sent}"
        );
        assert!(!sent.contains("Keep-Alive"), "{sent}");
        assert!(sent.ends_with("Connection: close\r\n\r\n"), "{sent}");
    }

    /// A stand-in for `pulp ui`: answers one request with `reply` and hands
    /// back what it received.
    fn fake_pulp(reply: &'static str) -> (SocketAddr, mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let head = super::super::read_head(&mut reader).unwrap();
            let mut body = vec![0; head.content_length() as usize];
            reader.read_exact(&mut body).unwrap();
            let mut stream = stream;
            stream.write_all(reply.as_bytes()).unwrap();
            let seen = format!(
                "{} {} host={} origin={} body={}",
                head.method,
                head.target,
                head.header("host").unwrap_or("-"),
                head.header("origin").unwrap_or("-"),
                String::from_utf8_lossy(&body)
            );
            // Some tests only need the answer, not what was sent.
            let _ = tx.send(seen);
        });
        (addr, rx)
    }

    fn exchange(addr: SocketAddr, request: &str) -> String {
        let mut stream = TcpStream::connect(addr).unwrap();
        stream.write_all(request.as_bytes()).unwrap();
        let mut reply = String::new();
        stream.read_to_string(&mut reply).unwrap();
        reply
    }

    #[test]
    fn test_forward_with_fake_pulp_rewrites_the_request_and_relays_the_answer() {
        const REPLY: &str = "HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok";
        let (pulp, seen) = fake_pulp(REPLY);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let own = listener.local_addr().unwrap();
        thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let head = super::super::read_head(&mut reader).unwrap();
            let mut body = vec![0; head.content_length() as usize];
            reader.read_exact(&mut body).unwrap();
            forward(stream, &head, &body, pulp, own).unwrap();
        });
        let request = format!(
            "POST /api/scan HTTP/1.1\r\nHost: {own}\r\nOrigin: http://{own}\r\n\
             Content-Length: 2\r\n\r\n{{}}"
        );
        assert_eq!(exchange(own, &request), REPLY);
        assert_eq!(
            seen.recv_timeout(Duration::from_secs(5)).unwrap(),
            format!("POST /api/scan host={pulp} origin=http://{pulp} body={{}}")
        );
    }

    #[test]
    fn test_forward_with_pulp_down_answers_bad_gateway() {
        let down = {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.local_addr().unwrap()
        };
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let own = listener.local_addr().unwrap();
        thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            // Read the request first, as the server does: closing a socket with
            // unread input resets the connection instead of ending it.
            let request = super::super::read_head(&mut BufReader::new(&stream)).unwrap();
            forward(stream, &request, b"", down, own).unwrap();
        });
        let reply = exchange(own, "GET / HTTP/1.1\r\n\r\n");
        assert!(reply.starts_with("HTTP/1.1 502 Bad Gateway\r\n"), "{reply}");
    }

    #[test]
    fn test_executable_from_messages_with_cargo_output_returns_the_pulp_binary() {
        let messages = [
            r#"{"reason":"compiler-artifact","target":{"kind":["custom-build"],"name":"build-script-build"},"executable":null,"fresh":true}"#,
            r#"{"reason":"build-script-executed","package_id":"x"}"#,
            r#"{"reason":"compiler-artifact","target":{"kind":["bin"],"name":"pulp"},"filenames":["/w/target/debug/pulp"],"executable":"/w/target/debug/pulp","fresh":false}"#,
            r#"{"reason":"build-finished","success":true}"#,
        ]
        .join("\n");
        assert_eq!(
            executable_from_messages(&messages),
            Some(PathBuf::from("/w/target/debug/pulp"))
        );
        assert_eq!(
            executable_from_messages(messages.lines().next().unwrap()),
            None
        );
        assert_eq!(executable_from_messages(""), None);
    }

    #[test]
    fn test_json_string_field_with_escapes_unescapes_the_value() {
        let line = r#"{"a":1,"executable": "C:\\t\"x\"\/\u00e9\ud83d\ude00\n"}"#;
        assert_eq!(
            json_string_field(line, "executable").as_deref(),
            Some("C:\\t\"x\"/\u{e9}\u{1f600}\n")
        );
        assert_eq!(
            json_string_field(r#"{"executable":null}"#, "executable"),
            None
        );
        assert_eq!(json_string_field(r#"{"other":"x"}"#, "executable"), None);
        assert_eq!(
            json_string_field(r#"{"executable":"cut"#, "executable"),
            None
        );
        assert_eq!(
            json_string_field(r#"{"executable":"\ud83d"}"#, "executable"),
            None
        );
        assert_eq!(
            json_string_field(r#"{"executable":"\q"}"#, "executable"),
            None
        );
    }

    #[test]
    fn test_json_quote_with_special_characters_round_trips() {
        let text = "a \"b\" \\c\\ \n\u{1} é";
        let quoted = json_quote(text);
        assert_eq!(quoted, r#""a \"b\" \\c\\ \u000a\u0001 é""#);
        let line = format!("{{\"k\":{quoted}}}");
        assert_eq!(json_string_field(&line, "k").as_deref(), Some(text));
    }

    #[test]
    fn test_fixtures_write_with_scratch_dir_makes_big_and_slow_folders() {
        let dir = private_scratch_dir().unwrap();
        let fixtures = Fixtures::write(Path::new("/w"), &dir).unwrap();
        let big: usize = (1..=BIG_PARTS)
            .map(|part| {
                std::fs::read_to_string(fixtures.big.join(format!("part-{part}.txt")))
                    .unwrap()
                    .len()
            })
            .sum();
        let last = std::fs::read_to_string(fixtures.big.join(format!("part-{BIG_PARTS}.txt")));
        let slow = std::fs::read_dir(&fixtures.slow).unwrap().count();
        let first_note = std::fs::read_to_string(fixtures.slow.join("note-000.rtf"));
        let json = format!("{{{}}}", fixtures.json_members());
        let _ = std::fs::remove_dir_all(&dir);

        assert!(
            big > 40 * 1024,
            "the big folder overflows the 32 KiB preview: {big}"
        );
        assert!(last.unwrap().trim_end().ends_with(BIG_END));
        assert_eq!(slow, SLOW_FILES);
        assert!(first_note.unwrap().starts_with("{\\rtf1"));
        assert_eq!(
            json_string_field(&json, "testdata").as_deref(),
            Some(Path::new("/w").join("testdata").to_str().unwrap())
        );
        assert_eq!(json_string_field(&json, "bigEnd").as_deref(), Some(BIG_END));
        assert!(
            json.contains(&format!("\"slowFiles\":{SLOW_FILES}")),
            "{json}"
        );
    }

    /// A script that stands in for `pulp ui`: it reports the address it was
    /// given, as pulp does, then waits.
    #[cfg(unix)]
    fn fake_pulp_exe(dir: &Path, script: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let exe = dir.join("pulp");
        std::fs::write(&exe, script).unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        exe
    }

    #[cfg(unix)]
    const SERVING: &str = "#!/bin/sh\n\
        while [ $# -gt 0 ]; do [ \"$1\" = --port ] && port=$2; shift; done\n\
        echo \"pulp mill on http://127.0.0.1:$port\" >&2\n\
        echo \"open http://127.0.0.1:$port/?token=t$port\" >&2\n\
        exec sleep 30\n";

    #[cfg(unix)]
    fn alive(pid: u32) -> bool {
        Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    #[cfg(unix)]
    #[test]
    fn test_pulp_ui_with_fake_binary_starts_restarts_and_stops() {
        let _serial = crate::uitest::interrupt_lock();
        let dir = private_scratch_dir().unwrap();
        let exe = fake_pulp_exe(&dir, SERVING);
        let log = Arc::new(Log::default());
        let mut pulp = PulpUi::start(exe, dir.clone(), Arc::clone(&log)).unwrap();
        let first = pulp.child.as_ref().unwrap().id();
        let first_port = pulp.port;
        let first_page = pulp.page.clone();
        pulp.stop();
        let first_gone = !alive(first);
        pulp.launch().unwrap();
        let second = pulp.child.as_ref().unwrap().id();
        let logged = log.text();
        pulp.stop();
        let second_gone = !alive(second);
        let _ = std::fs::remove_dir_all(&dir);

        assert_ne!(first_port, 0);
        assert_eq!(first_page, format!("/?token=t{first_port}"));
        assert!(first_gone && second_gone, "stop ends each pulp ui");
        assert_ne!(first, second, "a restart is a new process");
        assert!(
            logged.contains(&format!("http://127.0.0.1:{first_port}")),
            "{logged}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_pulp_ui_with_bare_address_takes_it_as_the_link() {
        let _serial = crate::uitest::interrupt_lock();
        let dir = private_scratch_dir().unwrap();
        let exe = fake_pulp_exe(
            &dir,
            "#!/bin/sh\n\
             while [ $# -gt 0 ]; do [ \"$1\" = --port ] && port=$2; shift; done\n\
             echo \"pulp mill on http://127.0.0.1:$port\" >&2\n\
             exec sleep 30\n",
        );
        let started = Instant::now();
        let pulp = PulpUi::start(exe, dir.clone(), Arc::default());
        let waited = started.elapsed();
        let page = match &pulp {
            Ok(pulp) => pulp.page.clone(),
            Err(err) => format!("no start: {err:#}"),
        };
        drop(pulp);
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(page, "/");
        assert!(
            waited >= LINK_GRACE,
            "it waited for a token link: {waited:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_pulp_ui_with_failing_binary_reports_its_log() {
        let _serial = crate::uitest::interrupt_lock();
        let dir = private_scratch_dir().unwrap();
        let exe = fake_pulp_exe(
            &dir,
            "#!/bin/sh\necho 'port is already in use' >&2\nexit 1\n",
        );
        let started = Instant::now();
        let failed = PulpUi::start(exe, dir.clone(), Arc::default());
        let _ = std::fs::remove_dir_all(&dir);

        let err = format!("{:#}", failed.err().unwrap());
        assert!(err.contains("already in use"), "{err}");
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn test_link_path_with_printed_links_returns_the_path_on_our_port() {
        let origin = "http://127.0.0.1:8971";
        assert_eq!(
            link_path("pulp mill on http://127.0.0.1:8971/?token=ab12", origin).as_deref(),
            Some("/?token=ab12")
        );
        assert_eq!(
            link_path("pulp mill on http://127.0.0.1:8971", origin).as_deref(),
            Some("/")
        );
        assert_eq!(
            link_path("pulp mill on http://127.0.0.1:89712/", origin),
            None
        );
        assert_eq!(link_path("127.0.0.1:8971 is already in use", origin), None);
        assert_eq!(link_path("", origin), None);
    }

    #[test]
    fn test_opener_policy_with_fake_pulp_reads_the_header() {
        const PAGE: &str = "HTTP/1.1 200 OK\r\ncross-origin-opener-policy: same-origin\r\ncontent-length: 0\r\n\r\n";
        let (pulp, seen) = fake_pulp(PAGE);
        assert_eq!(opener_policy(pulp).unwrap().as_deref(), Some("same-origin"));
        let asked = seen.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(asked, format!("GET / host={pulp} origin=- body="));
        let (plain, _) = fake_pulp("HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n");
        assert_eq!(opener_policy(plain).unwrap(), None);
        let (garbled, _) = fake_pulp("SSH-2.0-nope\r\n\r\n");
        assert!(opener_policy(garbled).is_err());
    }

    #[test]
    fn test_poll_hold_with_a_poll_out_holds_it_until_release() {
        let polls = Arc::new(PollHold::default());
        assert!(
            !polls.hold(Duration::from_millis(50)),
            "no poll came to hold"
        );
        let started = Instant::now();
        polls.pass();
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "a hold that found no poll stops holding"
        );

        let poll = {
            let polls = Arc::clone(&polls);
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(50));
                polls.pass();
            })
        };
        assert!(polls.hold(Duration::from_secs(5)), "the poll is held");
        thread::sleep(Duration::from_millis(100));
        let still_held = !poll.is_finished();
        polls.release();
        poll.join().unwrap();
        assert!(still_held, "a held poll waits for the release");
    }

    #[test]
    fn test_log_with_long_output_keeps_the_end() {
        let log = Log::default();
        for line in 0..2000 {
            log.push(&format!("line {line:04} é"));
        }
        let text = log.text();
        assert!(text.len() <= LOG_KEEP, "{}", text.len());
        assert!(text.ends_with("line 1999 é\n"));
    }

    #[test]
    fn test_mill_server_with_fake_pulp_serves_web_itself_and_forwards_the_rest() {
        const REPLY: &str = "HTTP/1.1 200 OK\r\ncontent-length: 5\r\n\r\nshell";
        let (pulp, seen) = fake_pulp(REPLY);
        let mill = Arc::new(Mill {
            pulp: Mutex::new(PulpUi {
                exe: PathBuf::new(),
                tmp: PathBuf::new(),
                child: None,
                port: pulp.port(),
                page: "/?token=t".into(),
                log: Arc::default(),
            }),
            log: Arc::default(),
            fixtures: "\"testdata\":\"/t\"".into(),
            opener_policy: Some("same-origin".into()),
            polls: PollHold::default(),
        });
        let server =
            Server::start(&crate::cargo::workspace_root(), Some(mill), &Arc::default()).unwrap();

        let js = exchange(server.addr, "GET /web/test/harness.js HTTP/1.1\r\n\r\n");
        assert!(js.starts_with("HTTP/1.1 200 OK\r\n"), "{js:.60}");
        assert!(
            js.contains("export function test"),
            "web/ is served from disk"
        );
        assert!(
            js.contains("\r\nCross-Origin-Opener-Policy: same-origin\r\n"),
            "the harness's own pages carry pulp's opener policy"
        );
        let env = exchange(server.addr, "GET /__ui-test/env HTTP/1.1\r\n\r\n");
        assert!(
            env.ends_with("\r\n\r\n{\"testdata\":\"/t\",\"page\":\"/?token=t\"}"),
            "{env}"
        );

        let shell = exchange(server.addr, "GET / HTTP/1.1\r\n\r\n");
        assert_eq!(shell, REPLY);
        let request = seen.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(request, format!("GET / host={pulp} origin=- body="));
    }
}
