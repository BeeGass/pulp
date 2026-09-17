//! Run hang-prone extractors in a child `pulp` process with a timeout.

use std::io::{Read, Write};
use std::path::Path;
#[cfg(not(target_arch = "wasm32"))]
use std::process::{Child, Command, ExitStatus, Stdio};
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

/// Isolation is on when `PULP_ISOLATE` is `1`/`true`, off when `0`/`false`.
/// Otherwise it follows [`running_as_pulp_bin`].
#[must_use]
pub fn should_isolate() -> bool {
    match std::env::var("PULP_ISOLATE") {
        Ok(value) => {
            let v = value.trim();
            v == "1" || v.eq_ignore_ascii_case("true")
        }
        Err(_) => running_as_pulp_bin(),
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
/// Always hands the child the parent-validated bytes via a temp file so the
/// child cannot reread a different filesystem path.
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
        let tmp = temp_extract_path();
        std::fs::write(&tmp, bytes)?;
        let result = spawn_extract(&tmp, kind, opts);
        let _ = std::fs::remove_file(&tmp);
        result
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn temp_extract_path() -> std::path::PathBuf {
    let mut buf = [0u8; 8];
    let _ = getrandom::fill(&mut buf);
    let name = buf.iter().fold(String::from("pulp-x-"), |mut s, b| {
        s.push_str(&format!("{b:02x}"));
        s
    });
    std::env::temp_dir().join(name)
}

#[cfg(not(target_arch = "wasm32"))]
fn spawn_extract(path: &Path, kind: Kind, opts: &ExtractOpts) -> Result<String, Error> {
    let exe = std::env::current_exe().map_err(|err| Error::msg(err.to_string()))?;
    let timeout = extract_timeout();
    let mut cmd = Command::new(exe);
    cmd.arg("__extract")
        .arg("--path")
        .arg(path)
        .arg("--kind")
        .arg(kind.as_str())
        .arg("--max-file-size")
        .arg(opts.max_file_size.to_string())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if opts.source_mode {
        cmd.arg("--source");
    }
    if opts.notebook_outputs {
        cmd.arg("--notebook-outputs");
    }
    let mut child = cmd.spawn().map_err(|err| Error::msg(err.to_string()))?;
    let (out, err, status) =
        wait_with_drain(&mut child, timeout, MAX_CHILD_STDOUT, MAX_CHILD_STDERR)?;
    if status.success() {
        String::from_utf8(out).map_err(|e| Error::msg(e.to_string()))
    } else {
        let msg = String::from_utf8_lossy(&err);
        Err(Error::msg(if msg.trim().is_empty() {
            format!("extractor exited {status}")
        } else {
            msg.trim().to_string()
        }))
    }
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

#[cfg(not(target_arch = "wasm32"))]
fn extract_timeout() -> Duration {
    std::env::var("PULP_EXTRACT_TIMEOUT_MS")
        .ok()
        .and_then(|s| s.parse().ok())
        .map(Duration::from_millis)
        .unwrap_or(DEFAULT_TIMEOUT)
}

/// Child-process entry used by [`spawn_extract`].
pub fn run_child(path: &Path, kind: Kind, opts: &ExtractOpts) -> Result<(), i32> {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(err) => {
            let _ = writeln!(std::io::stderr(), "{err}");
            return Err(2);
        }
    };
    let mut bytes = Vec::new();
    let n = match file
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
    match crate::extract::extract(&path.to_string_lossy(), &bytes, kind, opts) {
        Ok(text) => {
            if let Err(err) = std::io::stdout().write_all(text.as_bytes()) {
                let _ = writeln!(std::io::stderr(), "{err}");
                return Err(2);
            }
            Ok(())
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
