//! Isolation via the real `pulp` binary (not the lib test harness).

use std::process::{Command, Stdio};
use std::time::Duration;

use pulp::extract::ExtractOpts;
use pulp::extract::isolate::wait_with_drain;
use pulp::{Error, Kind};

const CORRUPT_PDF: &[u8] = b"%PDF-1.4 garbage";
const CORRUPT_DOCX: &[u8] = b"PK\x03\x04 not really a zip";

/// The parser message for `bytes` when extracted in this process.
fn unreadable_reason(name: &str, bytes: &[u8], kind: Kind) -> String {
    let opts = ExtractOpts {
        max_file_size: 1024 * 1024,
        notebook_outputs: false,
        source_mode: false,
    };
    match pulp::extract::extract(name, bytes, kind, &opts) {
        Err(Error::Unreadable(reason)) => reason,
        other => panic!("{name} should be unreadable, got {other:?}"),
    }
}

#[test]
fn test_pulp_extract_child_with_corrupt_pdf_returns_unreadable_exit_and_message() {
    let exe = env!("CARGO_BIN_EXE_pulp");
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("paper.pdf");
    std::fs::write(&path, CORRUPT_PDF).unwrap();
    let output = Command::new(exe)
        .args([
            "__extract",
            "--path",
            path.to_str().unwrap(),
            "--kind",
            "pdf",
        ])
        .output()
        .expect("spawn pulp __extract");
    assert_eq!(output.status.code(), Some(3), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        stderr.trim(),
        unreadable_reason("paper.pdf", CORRUPT_PDF, Kind::Pdf)
    );
}

#[test]
fn test_pulp_cli_with_corrupt_pdf_and_docx_returns_notes_and_unreadable_count() {
    let exe = env!("CARGO_BIN_EXE_pulp");
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("docs");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("paper.pdf"), CORRUPT_PDF).unwrap();
    std::fs::write(root.join("memo.docx"), CORRUPT_DOCX).unwrap();
    std::fs::write(root.join("notes.txt"), "tide tables\n").unwrap();
    let out = dir.path().join("dump.txt");
    let output = Command::new(exe)
        .env("PULP_ISOLATE", "1")
        .args(["-o", out.to_str().unwrap(), root.to_str().unwrap()])
        .output()
        .expect("spawn pulp");
    assert!(output.status.success(), "{output:?}");

    let dump = std::fs::read_to_string(&out).unwrap();
    let pdf = unreadable_reason("paper.pdf", CORRUPT_PDF, Kind::Pdf);
    let docx = unreadable_reason("memo.docx", CORRUPT_DOCX, Kind::Docx);
    assert!(dump.contains(&format!("[pdf unreadable: {pdf}]")), "{dump}");
    assert!(
        dump.contains(&format!("[docx unreadable: {docx}]")),
        "{dump}"
    );
    assert!(dump.contains("tide tables"), "{dump}");
    let summary = String::from_utf8_lossy(&output.stderr);
    assert!(summary.starts_with("pulped 1 files ("), "{summary}");
    assert!(summary.trim_end().ends_with(", 2 unreadable"), "{summary}");
}

const DOWNLOAD_PAGE: &[u8] = b"<html><head><title>Preparing to download ...</title></head>\
    <body><p>Your download will start in a moment.</p></body></html>\n";

const DOWNLOAD_NOTE: &str = "[not a PDF: it holds an HTML page (\"Preparing to download ...\")]";

#[test]
fn test_pulp_extract_child_with_html_page_named_pdf_returns_its_text_under_a_note() {
    use std::io::Write;
    let exe = env!("CARGO_BIN_EXE_pulp");
    let mut child = Command::new(exe)
        .args(["__extract", "--stdin", "--kind", "pdf"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn pulp __extract");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(DOWNLOAD_PAGE)
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "{output:?}");
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.starts_with(DOWNLOAD_NOTE), "{text}");
    assert!(
        text.contains("Your download will start in a moment."),
        "{text}"
    );
}

#[test]
fn test_pulp_cli_isolated_with_html_page_named_pdf_packs_its_text_under_a_note() {
    let exe = env!("CARGO_BIN_EXE_pulp");
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("downloads");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("report.pdf"), DOWNLOAD_PAGE).unwrap();
    let out = dir.path().join("dump.txt");
    let output = Command::new(exe)
        .env("PULP_ISOLATE", "1")
        .args(["-o", out.to_str().unwrap(), root.to_str().unwrap()])
        .output()
        .expect("spawn pulp");
    assert!(output.status.success(), "{output:?}");
    let dump = std::fs::read_to_string(&out).unwrap();
    assert!(dump.contains(&format!("{DOWNLOAD_NOTE}\n")), "{dump}");
    assert!(
        dump.contains("Your download will start in a moment."),
        "{dump}"
    );
    let summary = String::from_utf8_lossy(&output.stderr);
    assert!(summary.contains("pulped 1 files"), "{summary}");
}

#[test]
fn test_pulp_extract_child_with_hello_rs_prints_source() {
    let exe = env!("CARGO_BIN_EXE_pulp");
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("hello.rs");
    std::fs::write(&path, "fn hello() {}\n").unwrap();
    let output = Command::new(exe)
        .args([
            "__extract",
            "--path",
            path.to_str().unwrap(),
            "--kind",
            "text",
            "--max-file-size",
            "1048576",
        ])
        .output()
        .expect("spawn pulp __extract");
    assert!(output.status.success(), "{:?}", output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("fn hello"), "{stdout}");
}

fn tiny_docx(text: &str) -> Vec<u8> {
    use std::io::{Cursor, Write};
    let xml = format!(
        "<?xml version=\"1.0\"?><w:document xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\"><w:body><w:p><w:r><w:t>{text}</w:t></w:r></w:p></w:body></w:document>"
    );
    let mut zw = zip::ZipWriter::new(Cursor::new(Vec::new()));
    zw.start_file(
        "word/document.xml",
        zip::write::SimpleFileOptions::default(),
    )
    .unwrap();
    zw.write_all(xml.as_bytes()).unwrap();
    zw.finish().unwrap().into_inner()
}

#[test]
fn test_pulp_extract_child_with_stdin_prints_extracted_text() {
    use std::io::Write;
    let exe = env!("CARGO_BIN_EXE_pulp");
    let mut child = Command::new(exe)
        .args(["__extract", "--stdin", "--kind", "docx"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn pulp __extract");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(&tiny_docx("from stdin"))
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout), "from stdin");
}

/// `TMPDIR` names the temp directory on Unix only; Windows reads `TMP`.
#[cfg(unix)]
#[test]
fn test_pulp_cli_isolated_with_unusable_temp_dir_still_extracts() {
    let exe = env!("CARGO_BIN_EXE_pulp");
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("docs");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("memo.docx"), tiny_docx("quarterly tides")).unwrap();
    let output = Command::new(exe)
        .env("PULP_ISOLATE", "1")
        .env("TMPDIR", dir.path().join("no-such-temp-dir"))
        .arg(&root)
        .output()
        .expect("spawn pulp");
    assert!(output.status.success(), "{output:?}");
    let dump = String::from_utf8_lossy(&output.stdout);
    assert!(dump.contains("quarterly tides"), "{dump}");
}

/// Run `cmd`, retrying while the executable is busy: a binary copied a
/// moment ago can still be open for writing in a child that another test
/// forked meanwhile, and Linux refuses to run it until that child execs.
fn output_retrying_busy(cmd: &mut Command) -> std::process::Output {
    for _ in 0..50 {
        match cmd.output() {
            Err(err) if err.kind() == std::io::ErrorKind::ExecutableFileBusy => {
                std::thread::sleep(Duration::from_millis(100));
            }
            other => return other.expect("spawn pulp"),
        }
    }
    panic!("the executable stayed busy");
}

#[test]
fn test_pulp_cli_with_renamed_binary_still_isolates_heavy_extractors() {
    let dir = tempfile::tempdir().unwrap();
    let renamed = dir
        .path()
        .join(format!("pulp-renamed{}", std::env::consts::EXE_SUFFIX));
    std::fs::copy(env!("CARGO_BIN_EXE_pulp"), &renamed).unwrap();
    let root = dir.path().join("docs");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("memo.docx"), tiny_docx("isolated text")).unwrap();
    // With a zero timeout an isolated extractor always times out, so the
    // note shows the child really ran.
    let output = output_retrying_busy(
        Command::new(&renamed)
            .env_remove("PULP_ISOLATE")
            .env("PULP_EXTRACT_TIMEOUT_MS", "0")
            .arg(&root),
    );
    assert!(output.status.success(), "{output:?}");
    let dump = String::from_utf8_lossy(&output.stdout);
    assert!(dump.contains("extractor timed out"), "{dump}");
    assert!(!dump.contains("isolated text"), "{dump}");
}

#[test]
fn test_pulp_extract_child_with_no_parent_watching_exits_on_its_own_deadline() {
    let exe = env!("CARGO_BIN_EXE_pulp");
    // Stdin stays open and empty, so the child waits the way a stuck parser
    // would, and no parent is watching the clock.
    let mut child = Command::new(exe)
        .env("PULP_EXTRACT_TIMEOUT_MS", "100")
        .args(["__extract", "--stdin", "--kind", "pdf"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn pulp __extract");
    let (out, err, status) = wait_with_drain(&mut child, Duration::from_secs(20), 1024, 64 * 1024)
        .expect("child must exit by itself");
    assert_eq!(status.code(), Some(4), "{status}");
    assert!(out.is_empty());
    assert!(
        String::from_utf8_lossy(&err).contains("past its 2100ms deadline"),
        "{}",
        String::from_utf8_lossy(&err)
    );
}

#[test]
fn test_wait_with_drain_with_dd_stdout_completes() {
    let mut child = Command::new("sh")
        .args(["-c", "dd if=/dev/zero bs=1024 count=1024 2>/dev/null"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let (out, _, status) = wait_with_drain(
        &mut child,
        Duration::from_secs(10),
        2 * 1024 * 1024,
        64 * 1024,
    )
    .expect("drain");
    assert!(status.success());
    assert_eq!(out.len(), 1024 * 1024);
}

#[test]
fn test_wait_with_drain_with_oversize_stdout_returns_error() {
    let mut child = Command::new("sh")
        .args(["-c", "dd if=/dev/zero bs=1024 count=64 2>/dev/null"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let err = wait_with_drain(&mut child, Duration::from_secs(10), 1024, 64 * 1024).unwrap_err();
    assert!(err.to_string().contains("exceeded budget"), "{err}");
}
