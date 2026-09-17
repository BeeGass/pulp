//! Isolation via the real `pulp` binary (not the lib test harness).

use std::process::{Command, Stdio};
use std::time::Duration;

use pulp::extract::isolate::wait_with_drain;

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
