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
