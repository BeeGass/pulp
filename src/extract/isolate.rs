//! Run hang-prone extractors in a child `pulp` process with a timeout.

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::classify::Kind;
use crate::error::Error;
use crate::extract::ExtractOpts;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

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

/// Extract a heavy format, spawning a child when running as `pulp`.
pub fn extract_heavy(
    path: Option<&Path>,
    bytes: &[u8],
    kind: Kind,
    opts: &ExtractOpts,
) -> Result<String, Error> {
    #[cfg(target_arch = "wasm32")]
    {
        let _ = path;
        return crate::extract::extract("file", bytes, kind, opts);
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        if !running_as_pulp_bin() {
            return crate::extract::extract(
                path.and_then(|p| p.to_str()).unwrap_or("file"),
                bytes,
                kind,
                opts,
            );
        }
        match path {
            Some(path) => spawn_extract(path, kind, opts),
            None => {
                let tmp = temp_extract_path();
                std::fs::write(&tmp, bytes)?;
                let result = spawn_extract(&tmp, kind, opts);
                let _ = std::fs::remove_file(&tmp);
                result
            }
        }
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
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let start = Instant::now();
    loop {
        match child
            .try_wait()
            .map_err(|err| Error::msg(err.to_string()))?
        {
            Some(status) => {
                let mut out = Vec::new();
                let mut err = Vec::new();
                if let Some(mut s) = stdout.take() {
                    let _ = s.read_to_end(&mut out);
                }
                if let Some(mut s) = stderr.take() {
                    let _ = s.read_to_end(&mut err);
                }
                if status.success() {
                    return String::from_utf8(out).map_err(|e| Error::msg(e.to_string()));
                }
                let msg = String::from_utf8_lossy(&err);
                return Err(Error::msg(if msg.trim().is_empty() {
                    format!("extractor exited {}", status)
                } else {
                    msg.trim().to_string()
                }));
            }
            None if start.elapsed() > timeout => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(Error::msg(format!(
                    "extractor timed out after {}s",
                    timeout.as_secs()
                )));
            }
            None => std::thread::sleep(Duration::from_millis(25)),
        }
    }
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
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) => {
            let _ = writeln!(std::io::stderr(), "{err}");
            return Err(2);
        }
    };
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
    }
}
