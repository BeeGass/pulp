//! Run hang-prone extractors in a child `pulp` process with a timeout.

use std::io::{Read, Write};
use std::path::Path;
#[cfg(not(target_arch = "wasm32"))]
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
#[cfg(not(target_arch = "wasm32"))]
use std::time::{Duration, Instant};

use crate::classify::Kind;
use crate::error::Error;
use crate::extract::ExtractOpts;

#[cfg(not(target_arch = "wasm32"))]
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
#[cfg(not(target_arch = "wasm32"))]
const MAX_CHILD_STDOUT: usize = 32 * 1024 * 1024;
#[cfg(not(target_arch = "wasm32"))]
const MAX_CHILD_STDERR: usize = 256 * 1024;
/// Exit code of a child whose parser rejected the file. Its stderr holds the
/// parser's message.
const UNREADABLE_EXIT: i32 = 3;
/// Exit code of a child that ran past its own deadline.
#[cfg(not(target_arch = "wasm32"))]
const WATCHDOG_EXIT: i32 = 4;
/// Time a child allows beyond the parent's timeout before exiting itself.
#[cfg(not(target_arch = "wasm32"))]
const WATCHDOG_GRACE: Duration = Duration::from_secs(2);

/// Kinds whose parsers can hang in native code.
#[must_use]
pub fn needs_isolation(kind: Kind) -> bool {
    matches!(
        kind,
        Kind::Pdf
            | Kind::Docx
            | Kind::Pptx
            | Kind::Spreadsheet
            | Kind::Odt
            | Kind::Odp
            | Kind::Epub
            | Kind::Rtf
    )
}

/// Isolation the host program chose: 0 none yet, 1 on, 2 off.
static ISOLATION: AtomicU8 = AtomicU8::new(0);

/// Set in a child extractor, which must never spawn children of its own.
static IN_CHILD: AtomicBool = AtomicBool::new(false);

/// Choose whether this process runs heavy extractors in a child.
///
/// The `pulp` binary turns isolation on at startup, so it holds whatever the
/// executable is named. `PULP_ISOLATE` still overrides this choice.
pub fn set_isolation(enabled: bool) {
    ISOLATION.store(if enabled { 1 } else { 2 }, Ordering::Relaxed);
}

/// Isolation is on when `PULP_ISOLATE` is `1`/`true`, off when it is set to
/// anything else. Otherwise it follows [`set_isolation`] and, when that was
/// never called, [`running_as_pulp_bin`]. A child extractor never isolates.
#[must_use]
pub fn should_isolate() -> bool {
    if IN_CHILD.load(Ordering::Relaxed) {
        return false;
    }
    match std::env::var("PULP_ISOLATE") {
        Ok(value) => {
            let v = value.trim();
            v == "1" || v.eq_ignore_ascii_case("true")
        }
        Err(_) => match ISOLATION.load(Ordering::Relaxed) {
            1 => true,
            2 => false,
            _ => running_as_pulp_bin(),
        },
    }
}

/// True when this process is the `pulp` CLI, not a test harness.
#[must_use]
#[cfg(not(target_arch = "wasm32"))]
pub fn running_as_pulp_bin() -> bool {
    std::env::current_exe()
        .ok()
        .and_then(|path| {
            path.file_stem()
                .and_then(|stem| stem.to_str())
                .map(|stem| stem == "pulp")
        })
        .unwrap_or(false)
}

#[cfg(target_arch = "wasm32")]
#[must_use]
pub fn running_as_pulp_bin() -> bool {
    false
}

/// Extract a heavy format, spawning a child when isolation is enabled.
///
/// The child gets the parent-validated bytes on its stdin, so it cannot
/// reread a different filesystem path, and no copy of the document is left
/// in a shared temp directory.
pub fn extract_heavy(
    path: Option<&Path>,
    bytes: &[u8],
    kind: Kind,
    opts: &ExtractOpts,
) -> Result<String, Error> {
    let _ = path;
    if bytes.len() as u64 > opts.max_file_size {
        return Err(Error::msg(format!(
            "file is {} bytes; limit {}",
            bytes.len(),
            opts.max_file_size
        )));
    }
    if !should_isolate() {
        return crate::extract::extract("file", bytes, kind, opts);
    }
    #[cfg(target_arch = "wasm32")]
    {
        crate::extract::extract("file", bytes, kind, opts)
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        spawn_extract(bytes, kind, opts)
    }
}

/// Extract `bytes` as `kind` in a child process, whatever the kind.
///
/// For extractors that run in this process unless a page looks risky, such
/// as HTML nested past what its parse model can vouch for.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn extract_in_child(bytes: &[u8], kind: Kind) -> Result<String, Error> {
    let opts = ExtractOpts {
        max_file_size: bytes.len() as u64,
        notebook_outputs: false,
        source_mode: false,
    };
    spawn_extract(bytes, kind, &opts)
}

#[cfg(not(target_arch = "wasm32"))]
fn spawn_extract(bytes: &[u8], kind: Kind, opts: &ExtractOpts) -> Result<String, Error> {
    let exe = std::env::current_exe().map_err(|err| Error::msg(err.to_string()))?;
    let timeout = extract_timeout();
    let mut cmd = Command::new(exe);
    cmd.arg("__extract")
        .arg("--stdin")
        .arg("--kind")
        .arg(kind.as_str())
        .arg("--max-file-size")
        .arg(opts.max_file_size.to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if opts.source_mode {
        cmd.arg("--source");
    }
    if opts.notebook_outputs {
        cmd.arg("--notebook-outputs");
    }
    let mut child = cmd.spawn().map_err(|err| Error::msg(err.to_string()))?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| Error::msg("extractor stdin not piped"))?;
    let drained = std::thread::scope(|scope| {
        // Feed the bytes beside the drain. A child that stops reading gets
        // killed by the timeout, which ends this write with a broken pipe.
        scope.spawn(move || {
            let mut stdin = stdin;
            let _ = stdin.write_all(bytes);
        });
        let drained = wait_with_drain(&mut child, timeout, MAX_CHILD_STDOUT, MAX_CHILD_STDERR);
        if drained.is_err() {
            let _ = child.kill();
            let _ = child.wait();
        }
        drained
    });
    let (out, err, status) = drained?;
    child_result(out, &err, status)
}

/// A finished child's text, or its failure. [`UNREADABLE_EXIT`] becomes
/// [`Error::Unreadable`], as the same failure in this process would.
#[cfg(not(target_arch = "wasm32"))]
fn child_result(out: Vec<u8>, err: &[u8], status: ExitStatus) -> Result<String, Error> {
    if status.success() {
        return String::from_utf8(out).map_err(|e| Error::msg(e.to_string()));
    }
    let msg = String::from_utf8_lossy(err);
    let msg = msg.trim();
    if status.code() == Some(UNREADABLE_EXIT) {
        return Err(Error::Unreadable(if msg.is_empty() {
            "the parser rejected the file".to_string()
        } else {
            msg.to_string()
        }));
    }
    Err(Error::msg(if msg.is_empty() {
        format!("extractor exited {status}")
    } else {
        msg.to_string()
    }))
}

/// Drain stdout/stderr while waiting so a completed child cannot block on a full pipe.
#[cfg(not(target_arch = "wasm32"))]
pub fn wait_with_drain(
    child: &mut Child,
    timeout: Duration,
    max_out: usize,
    max_err: usize,
) -> Result<(Vec<u8>, Vec<u8>, ExitStatus), Error> {
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| Error::msg("extractor stdout not piped"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| Error::msg("extractor stderr not piped"))?;
    let out_h = std::thread::spawn(move || read_capped(stdout, max_out));
    let err_h = std::thread::spawn(move || read_capped(stderr, max_err));
    let start = Instant::now();
    let status = loop {
        match child
            .try_wait()
            .map_err(|err| Error::msg(err.to_string()))?
        {
            Some(status) => break status,
            None if start.elapsed() > timeout => {
                let _ = child.kill();
                let _status = child.wait().map_err(|err| Error::msg(err.to_string()))?;
                let _ = out_h.join();
                let _ = err_h.join();
                return Err(Error::msg(format!(
                    "extractor timed out after {}s",
                    timeout.as_secs()
                )));
            }
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    };
    let out = out_h
        .join()
        .map_err(|_| Error::msg("extractor stdout reader panicked"))?;
    let err = err_h
        .join()
        .map_err(|_| Error::msg("extractor stderr reader panicked"))?;
    match (out, err) {
        (Ok(out), Ok(err)) => Ok((out, err, status)),
        (Err(err), _) | (_, Err(err)) => {
            let _ = child.kill();
            Err(err)
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn read_capped<R: Read>(mut reader: R, max: usize) -> Result<Vec<u8>, Error> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let n = reader
            .read(&mut chunk)
            .map_err(|err| Error::msg(err.to_string()))?;
        if n == 0 {
            break;
        }
        if buf.len().saturating_add(n) > max {
            let mut sink = [0u8; 8192];
            while reader.read(&mut sink).unwrap_or(0) > 0 {}
            return Err(Error::msg("extractor output exceeded budget"));
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    Ok(buf)
}

/// Make a panic in this child extractor print only `extractor panicked:
/// <message>`, the words a parser that panics in the parent leaves in its
/// note. The parent passes the child's stderr on as the note, which then
/// carries no thread id or source location and reads the same whether the
/// parser ran in a child, in the parent, or in the browser.
pub fn set_child_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let _ = writeln!(
            std::io::stderr(),
            "extractor panicked: {}",
            panic_message(info.payload())
        );
    }));
}

/// The message a panic carried, or `unknown panic` when it held none.
pub(crate) fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        return (*message).to_string();
    }
    if let Some(message) = payload.downcast_ref::<String>() {
        return message.clone();
    }
    "unknown panic".to_string()
}

/// Make this child process exit once the extraction timeout, plus a short
/// grace, has passed.
///
/// The parent kills a child that runs over, but a parent that is itself
/// killed cannot, and a parser stuck in a loop never notices. This keeps
/// such a child from outliving the run that started it.
#[cfg(not(target_arch = "wasm32"))]
pub fn start_child_watchdog() {
    let deadline = extract_timeout().saturating_add(WATCHDOG_GRACE);
    std::thread::spawn(move || {
        std::thread::sleep(deadline);
        let _ = writeln!(
            std::io::stderr(),
            "extractor ran past its {}ms deadline",
            deadline.as_millis()
        );
        std::process::exit(WATCHDOG_EXIT);
    });
}

#[cfg(not(target_arch = "wasm32"))]
fn extract_timeout() -> Duration {
    std::env::var("PULP_EXTRACT_TIMEOUT_MS")
        .ok()
        .and_then(|s| s.parse().ok())
        .map(Duration::from_millis)
        .unwrap_or(DEFAULT_TIMEOUT)
}

/// Child-process entry for a file named on the command line.
pub fn run_child(path: &Path, kind: Kind, opts: &ExtractOpts) -> Result<(), i32> {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(err) => {
            let _ = writeln!(std::io::stderr(), "{err}");
            return Err(2);
        }
    };
    run_child_reader(file, &path.to_string_lossy(), kind, opts)
}

/// Child-process entry used by [`spawn_extract`]: extract the bytes read
/// from `input` and print the text on stdout.
///
/// Exits with [`UNREADABLE_EXIT`] when the parser rejects the bytes, and 2
/// for any other failure; stderr holds the message.
pub fn run_child_reader<R: Read>(
    input: R,
    name: &str,
    kind: Kind,
    opts: &ExtractOpts,
) -> Result<(), i32> {
    // An extractor that would isolate part of its work runs it here; a
    // child that spawned children could recurse without end.
    IN_CHILD.store(true, Ordering::Relaxed);
    let mut bytes = Vec::new();
    let n = match input
        .take(opts.max_file_size.saturating_add(1))
        .read_to_end(&mut bytes)
    {
        Ok(n) => n,
        Err(err) => {
            let _ = writeln!(std::io::stderr(), "{err}");
            return Err(2);
        }
    };
    if n as u64 > opts.max_file_size {
        let _ = writeln!(
            std::io::stderr(),
            "file is {n} bytes; limit {}",
            opts.max_file_size
        );
        return Err(2);
    }
    match crate::extract::extract(name, &bytes, kind, opts) {
        Ok(text) => {
            if let Err(err) = std::io::stdout().write_all(text.as_bytes()) {
                let _ = writeln!(std::io::stderr(), "{err}");
                return Err(2);
            }
            Ok(())
        }
        Err(Error::Unreadable(reason)) => {
            let _ = writeln!(std::io::stderr(), "{reason}");
            Err(UNREADABLE_EXIT)
        }
        Err(err) => {
            let _ = writeln!(std::io::stderr(), "{err}");
            Err(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_needs_isolation_with_pdf_returns_true() {
        assert!(needs_isolation(Kind::Pdf));
        assert!(needs_isolation(Kind::Docx));
        assert!(!needs_isolation(Kind::Text));
        assert!(!needs_isolation(Kind::Html));
    }

    #[test]
    fn test_running_as_pulp_bin_in_test_harness_returns_false() {
        assert!(
            !running_as_pulp_bin(),
            "lib tests run as pulp-<hash>, not pulp"
        );
        assert!(
            !should_isolate(),
            "tests must not isolate unless PULP_ISOLATE=1"
        );
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn finished(script: &str) -> (Vec<u8>, Vec<u8>, ExitStatus) {
        let mut child = Command::new("sh")
            .args(["-c", script])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn sh");
        wait_with_drain(&mut child, Duration::from_secs(10), 1024, 1024).expect("drain")
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn test_child_result_with_unreadable_exit_returns_unreadable_error() {
        let (out, err, status) = finished("echo 'invalid file header' >&2; exit 3");
        match child_result(out, &err, status) {
            Err(Error::Unreadable(reason)) => assert_eq!(reason, "invalid file header"),
            other => panic!("expected an unreadable error, got {other:?}"),
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn test_child_result_with_other_failure_returns_message_error() {
        let (out, err, status) = finished("echo 'thread panicked' >&2; exit 101");
        match child_result(out, &err, status) {
            Err(Error::Message(msg)) => assert_eq!(msg, "thread panicked"),
            other => panic!("expected a message error, got {other:?}"),
        }
        let (out, err, status) = finished("printf 'text'");
        assert_eq!(child_result(out, &err, status).unwrap(), "text");
    }

    #[test]
    fn test_read_capped_with_oversize_returns_error() {
        let data = vec![b'x'; 100];
        let err = read_capped(data.as_slice(), 40).unwrap_err();
        assert!(err.to_string().contains("exceeded budget"), "{err}");
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn test_wait_with_drain_with_large_stdout_returns_before_timeout() {
        let mut child = Command::new("sh")
            .args(["-c", "dd if=/dev/zero bs=1024 count=2048 2>/dev/null"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn dd");
        let (out, _, status) = wait_with_drain(
            &mut child,
            Duration::from_secs(10),
            4 * 1024 * 1024,
            64 * 1024,
        )
        .expect("drain");
        assert!(status.success(), "{status}");
        assert_eq!(out.len(), 2048 * 1024);
    }
}
