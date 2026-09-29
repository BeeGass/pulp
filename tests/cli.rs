//! The `pulp` binary end to end: output routing, exit codes, and dumps that
//! must stay byte-identical across runs.

use std::fs;
#[cfg(unix)]
use std::io::Read;
use std::path::Path;
#[cfg(unix)]
use std::process::Stdio;
use std::process::{Command, Output};

fn pulp() -> Command {
    Command::new(env!("CARGO_BIN_EXE_pulp"))
}

fn run(cmd: &mut Command) -> Output {
    cmd.output().expect("spawn pulp")
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn write(path: &Path, body: &[u8]) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, body).unwrap();
}

#[cfg(unix)]
#[test]
fn test_pulp_with_reader_closing_pipe_early_exits_quietly() {
    let dir = tempfile::tempdir().unwrap();
    for i in 0..400 {
        write(
            &dir.path().join(format!("src/f{i:03}.rs")),
            "fn body() { let x = 1; }\n".repeat(40).as_bytes(),
        );
    }
    let mut child = pulp()
        .arg(dir.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let mut first = [0u8; 64];
    stdout.read_exact(&mut first).unwrap();
    drop(stdout);
    let out = child.wait_with_output().unwrap();
    let err = stderr(&out);
    assert!(out.status.success(), "{:?} {err}", out.status);
    assert!(!err.contains("Broken pipe"), "{err}");
    assert!(!err.contains("panicked"), "{err}");
}

#[cfg(unix)]
#[test]
fn test_pulp_with_stderr_closed_early_exits_zero() {
    let dir = tempfile::tempdir().unwrap();
    write(&dir.path().join("a.rs"), b"fn a() {}\n");
    let mut child = pulp()
        .arg(dir.path())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    drop(child.stderr.take());
    let status = child.wait().unwrap();
    assert_eq!(status.code(), Some(0), "{status:?}");
}

#[cfg(unix)]
#[test]
fn test_pulp_with_quiet_and_unreadable_directory_still_prints_warnings() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    write(&dir.path().join("a.rs"), b"fn a() {}\n");
    let locked = dir.path().join("locked");
    write(&locked.join("inner.rs"), b"fn hidden() {}\n");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    let privileged = fs::read_dir(&locked).is_ok();
    let out = run(pulp().arg("-q").arg(dir.path()));
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
    if privileged {
        // Running as root: nothing is unreadable, so there is nothing to test.
        return;
    }
    assert!(out.status.success(), "{}", stderr(&out));
    let err = stderr(&out);
    assert!(err.starts_with("warning: "), "{err}");
    assert!(err.contains("locked"), "{err}");
    assert!(!err.contains("pulped"), "{err}");
}

#[cfg(unix)]
#[test]
fn test_pulp_with_unreadable_directory_warns_and_packs_the_rest() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    write(&dir.path().join("a.rs"), b"fn a() {}\n");
    let locked = dir.path().join("locked");
    write(&locked.join("inner.rs"), b"fn hidden() {}\n");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    let privileged = fs::read_dir(&locked).is_ok();
    let out = run(pulp().arg(dir.path()));
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
    if privileged {
        // Running as root: nothing is unreadable, so there is nothing to test.
        return;
    }
    assert!(out.status.success(), "{}", stderr(&out));
    let err = stderr(&out);
    assert!(err.starts_with("warning: "), "{err}");
    assert!(err.contains("locked"), "{err}");
    let dump = String::from_utf8(out.stdout).unwrap();
    assert!(dump.contains("FILE: a.rs"), "{dump}");
}

#[test]
fn test_pulp_with_missing_root_exits_one_with_message() {
    let out = run(pulp().arg("/nonexistent/pulp-cli-missing-root"));
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("does not exist"), "{}", stderr(&out));
    assert!(out.stdout.is_empty());
}

#[test]
fn test_pulp_with_any_job_count_returns_identical_dump() {
    let dir = tempfile::tempdir().unwrap();
    for i in 0..60 {
        write(
            &dir.path().join(format!("pkg{}/m{i:02}.py", i % 7)),
            format!("VALUE_{i} = {i}\n").as_bytes(),
        );
    }
    fs::create_dir_all(dir.path().join("weights")).unwrap();
    for i in 0..3 {
        let file = fs::File::create(dir.path().join(format!("weights/w{i}.safetensors")));
        file.unwrap().set_len(600 * 1024 * 1024).unwrap();
    }
    let mut dumps = Vec::new();
    for jobs in ["1", "2", "8", "1", "8"] {
        let out = run(pulp().args(["-q", "-f", "xml", "-j", jobs]).arg(dir.path()));
        assert!(out.status.success(), "{}", stderr(&out));
        dumps.push(out.stdout);
    }
    assert!(dumps.windows(2).all(|w| w[0] == w[1]));
    let dump = String::from_utf8(dumps.remove(0)).unwrap();
    assert_eq!(dump.matches("<document index=").count(), 63, "{dump}");
    assert!(dump.contains("VALUE_59 = 59"), "{dump}");
}

#[cfg(unix)]
#[test]
fn test_pulp_with_control_bytes_in_names_and_content_writes_well_formed_xml() {
    let dir = tempfile::tempdir().unwrap();
    write(
        &dir.path().join("ctrl.txt"),
        b"bell\x07 esc\x1b vt\x0b ok\n",
    );
    write(&dir.path().join("line\nbreak.md"), b"# t\n");
    let out = run(pulp().args(["-q", "-f", "xml"]).arg(dir.path()));
    assert!(out.status.success(), "{}", stderr(&out));
    let xml = String::from_utf8(out.stdout).unwrap();
    let forbidden: Vec<char> = xml
        .chars()
        .filter(|c| c.is_control() && !matches!(c, '\t' | '\n' | '\r'))
        .filter(|c| (*c as u32) < 0x20)
        .collect();
    assert!(forbidden.is_empty(), "{forbidden:?} in {xml:?}");
    assert!(xml.contains("<source>line\\nbreak.md</source>"), "{xml}");
}
