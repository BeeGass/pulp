//! The `pulp` binary end to end: output routing, exit codes, and dumps that
//! must stay byte-identical across runs.

use std::fs;
#[cfg(unix)]
use std::io::Read;
use std::path::Path;
#[cfg(unix)]
use std::process::Stdio;
use std::process::{Command, Output};
#[cfg(unix)]
use std::time::Duration;

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

/// Names in `dir`, sorted.
fn names_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// `pulp` run under a 1 KiB file-size limit with `SIGXFSZ` ignored, so any
/// write that grows a file past it fails with `EFBIG`.
#[cfg(unix)]
fn pulp_with_tiny_file_limit() -> Command {
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg("ulimit -f 1; trap '' XFSZ; exec \"$0\" \"$@\"")
        .arg(env!("CARGO_BIN_EXE_pulp"));
    cmd
}

/// Wait for `child`, killing it and failing the test past `limit`.
#[cfg(unix)]
fn wait_or_kill(child: std::process::Child, limit: Duration) -> Output {
    let pid = child.id();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });
    match rx.recv_timeout(limit) {
        Ok(out) => out.expect("wait for pulp"),
        Err(_) => {
            let _ = Command::new("kill").arg("-9").arg(pid.to_string()).status();
            panic!("pulp did not finish within {limit:?}");
        }
    }
}

/// A folder of `count` Rust files, about 1 KiB each.
#[cfg(unix)]
fn many_files(root: &Path, count: usize) {
    for i in 0..count {
        write(
            &root.join(format!("src/f{i:03}.rs")),
            "fn body() { let x = 1; }\n".repeat(40).as_bytes(),
        );
    }
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

#[test]
fn test_pulp_list_with_output_writes_escaped_paths_to_file() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    write(&root.join("a.rs"), b"fn a() {}\n");
    write(&root.join("odd\nname.txt"), b"x\n");
    let list = dir.path().join("list.txt");
    let out = run(pulp().arg("--list").arg("-o").arg(&list).arg(&root));
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(out.stdout.is_empty(), "{:?}", out.stdout);
    let listed = fs::read_to_string(&list).unwrap();
    assert_eq!(listed, "a.rs\nodd\\nname.txt\n");
    assert!(
        stderr(&out).starts_with("listed 2 files in "),
        "{}",
        stderr(&out)
    );
}

#[cfg(unix)]
#[test]
fn test_pulp_with_stdout_redirected_into_root_leaves_dump_out() {
    let dir = tempfile::tempdir().unwrap();
    write(&dir.path().join("a.rs"), b"fn a() {}\n");
    let dump_path = dir.path().join("dump.txt");
    let dump = fs::File::create(&dump_path).unwrap();
    let out = run(pulp().arg(dir.path()).stdout(Stdio::from(dump)));
    assert!(out.status.success(), "{}", stderr(&out));
    let text = fs::read_to_string(&dump_path).unwrap();
    assert!(text.contains("FILE: a.rs"), "{text}");
    assert!(!text.contains("dump.txt"), "{text}");
}

#[test]
fn test_pulp_with_quiet_and_tokens_prints_only_the_estimate() {
    let dir = tempfile::tempdir().unwrap();
    write(&dir.path().join("a.rs"), b"fn a() {}\n");
    let out = run(pulp().args(["-q", "--tokens"]).arg(dir.path()));
    assert!(out.status.success(), "{}", stderr(&out));
    let err = stderr(&out);
    assert!(
        err.starts_with('~') && err.trim_end().ends_with(" tokens"),
        "{err}"
    );
    assert_eq!(err.lines().count(), 1, "{err}");
    let quiet = run(pulp().arg("-q").arg(dir.path()));
    assert!(quiet.stderr.is_empty(), "{}", stderr(&quiet));
}

#[test]
fn test_pulp_with_output_equal_to_input_file_refuses_and_keeps_input() {
    let dir = tempfile::tempdir().unwrap();
    let notes = dir.path().join("notes.txt");
    write(&notes, b"irreplaceable\n");
    let out = run(pulp().arg("-o").arg(&notes).arg(&notes));
    assert!(!out.status.success());
    assert!(stderr(&out).contains("also an input"), "{}", stderr(&out));
    assert_eq!(fs::read_to_string(&notes).unwrap(), "irreplaceable\n");
}

#[test]
fn test_pulp_with_missing_output_dir_fails_before_packing() {
    let dir = tempfile::tempdir().unwrap();
    // A root that does not exist: had the walk run first, it would fail on it.
    let out = run(pulp()
        .arg("-o")
        .arg(dir.path().join("nope/dump.txt"))
        .arg(dir.path().join("missing-root")));
    assert_eq!(out.status.code(), Some(1));
    let err = stderr(&out);
    assert!(err.contains("output directory"), "{err}");
    assert!(err.contains("nope does not exist"), "{err}");
    assert!(!err.contains("missing-root"), "{err}");
}

#[test]
fn test_pulp_with_output_under_a_file_fails_before_packing() {
    let dir = tempfile::tempdir().unwrap();
    write(&dir.path().join("afile"), b"x\n");
    let out = run(pulp()
        .arg("-o")
        .arg(dir.path().join("afile/dump.txt"))
        .arg(dir.path().join("missing-root")));
    assert_eq!(out.status.code(), Some(1));
    let err = stderr(&out);
    assert!(err.contains("afile is not a directory"), "{err}");
    assert!(!err.contains("missing-root"), "{err}");
}

#[cfg(unix)]
#[test]
fn test_pulp_with_read_only_output_dir_fails_before_packing() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let ro = dir.path().join("ro");
    fs::create_dir(&ro).unwrap();
    fs::set_permissions(&ro, fs::Permissions::from_mode(0o555)).unwrap();
    let privileged = fs::File::create(ro.join("probe")).is_ok();
    let out = run(pulp()
        .arg("-o")
        .arg(ro.join("dump.txt"))
        .arg(dir.path().join("missing-root")));
    fs::set_permissions(&ro, fs::Permissions::from_mode(0o755)).unwrap();
    if privileged {
        // Running as root: the directory is writable, so there is nothing to test.
        return;
    }
    assert_eq!(out.status.code(), Some(1));
    let err = stderr(&out);
    assert!(
        err.contains("cannot create a file in output directory"),
        "{err}"
    );
    assert!(err.contains("Permission denied"), "{err}");
    assert!(!err.contains("missing-root"), "{err}");
    assert!(names_in(&ro).is_empty(), "{:?}", names_in(&ro));
}

/// Windows caps a whole path at 260 characters unless long paths are on,
/// so the name limit this checks is a Unix one.
#[cfg(unix)]
#[test]
fn test_pulp_with_250_byte_output_name_writes_the_dump() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    write(&root.join("a.rs"), b"fn a() {}\n");
    let name = format!("{}.txt", "d".repeat(246));
    let dump = dir.path().join(&name);
    let out = run(pulp().arg("-o").arg(&dump).arg(&root));
    assert!(out.status.success(), "{}", stderr(&out));
    let text = fs::read_to_string(&dump).unwrap();
    assert!(text.contains("FILE: a.rs"), "{text}");
    assert_eq!(names_in(dir.path()), [name, "root".to_string()]);
}

#[cfg(unix)]
#[test]
fn test_pulp_with_output_and_failed_write_keeps_old_dump_and_symlink() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    many_files(&root, 20);
    let out_dir = dir.path().join("out");
    let real = out_dir.join("real.txt");
    write(&real, b"old dump\n");
    let link = out_dir.join("link.txt");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    for target in [&link, &real] {
        let out = run(pulp_with_tiny_file_limit().arg("-o").arg(target).arg(&root));
        assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
        assert!(stderr(&out).contains("too large"), "{}", stderr(&out));
        assert_eq!(fs::read_link(&link).unwrap(), real);
        assert_eq!(fs::read_to_string(&real).unwrap(), "old dump\n");
        assert_eq!(names_in(&out_dir), ["link.txt", "real.txt"]);
    }
}

#[cfg(unix)]
#[test]
fn test_pulp_with_existing_output_replaces_it_and_keeps_its_mode() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    write(&root.join("a.rs"), b"fn a() {}\n");
    let dump = dir.path().join("dump.txt");
    write(&dump, b"old dump\n");
    fs::set_permissions(&dump, fs::Permissions::from_mode(0o640)).unwrap();
    let out = run(pulp().arg("-o").arg(&dump).arg(&root));
    assert!(out.status.success(), "{}", stderr(&out));
    let text = fs::read_to_string(&dump).unwrap();
    assert!(text.contains("FILE: a.rs"), "{text}");
    assert!(!text.contains("old dump"), "{text}");
    let mode = fs::metadata(&dump).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o640);
    assert_eq!(names_in(dir.path()), ["dump.txt", "root"]);
}

#[test]
fn test_pulp_with_output_inside_root_leaves_old_dump_out() {
    let dir = tempfile::tempdir().unwrap();
    write(&dir.path().join("a.rs"), b"fn a() {}\n");
    let dump = dir.path().join("dump.txt");
    write(&dump, b"old dump\n");
    let out = run(pulp().arg("-o").arg(&dump).arg(dir.path()));
    assert!(out.status.success(), "{}", stderr(&out));
    let text = fs::read_to_string(&dump).unwrap();
    assert!(text.contains("FILE: a.rs"), "{text}");
    assert!(!text.contains("dump.txt"), "{text}");
    assert_eq!(names_in(dir.path()), ["a.rs", "dump.txt"]);
}

#[cfg(unix)]
#[test]
fn test_pulp_with_output_fifo_and_reader_closing_early_keeps_the_fifo() {
    use std::os::unix::fs::FileTypeExt;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    many_files(&root, 200);
    let fifo = dir.path().join("dump.fifo");
    let made = Command::new("mkfifo").arg(&fifo).status().unwrap();
    assert!(made.success());
    let reader_fifo = fifo.clone();
    std::thread::spawn(move || {
        // Blocks until pulp opens the FIFO, then hangs up after a few bytes.
        let mut file = fs::File::open(&reader_fifo).unwrap();
        let mut first = [0u8; 16];
        file.read_exact(&mut first).unwrap();
    });
    let child = pulp()
        .arg("-o")
        .arg(&fifo)
        .arg(&root)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let out = wait_or_kill(child, Duration::from_secs(60));
    let err = stderr(&out);
    assert!(out.status.success(), "{:?} {err}", out.status);
    assert!(!err.contains("Broken pipe"), "{err}");
    let kind = fs::symlink_metadata(&fifo).unwrap().file_type();
    assert!(kind.is_fifo(), "the FIFO must stay a FIFO: {kind:?}");
}

#[cfg(unix)]
#[test]
fn test_pulp_with_output_dev_null_packs_and_succeeds() {
    use std::os::unix::fs::FileTypeExt;
    let dir = tempfile::tempdir().unwrap();
    write(&dir.path().join("a.rs"), b"fn a() {}\n");
    let out = run(pulp().args(["-o", "/dev/null"]).arg(dir.path()));
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stderr(&out).starts_with("pulped 1 files"),
        "{}",
        stderr(&out)
    );
    let null = fs::metadata("/dev/null").unwrap();
    assert!(null.file_type().is_char_device());
}

#[cfg(unix)]
#[test]
fn test_pulp_with_stdout_redirected_into_root_and_max_entries_packs_inputs() {
    let dir = tempfile::tempdir().unwrap();
    write(&dir.path().join("a.rs"), b"fn a() {}\n");
    // Sorts before `a.rs`, so it would take the only entry if it counted.
    let dump_path = dir.path().join("0dump.txt");
    let dump = fs::File::create(&dump_path).unwrap();
    let out = run(pulp()
        .args(["--max-entries", "1"])
        .arg(dir.path())
        .stdout(Stdio::from(dump)));
    assert!(out.status.success(), "{}", stderr(&out));
    let text = fs::read_to_string(&dump_path).unwrap();
    assert!(text.contains("FILE: a.rs"), "{text}");
    assert!(!text.contains("0dump.txt"), "{text}");
}

#[cfg(unix)]
#[test]
fn test_pulp_with_output_dev_stdout_redirected_into_root_leaves_dump_out() {
    let dir = tempfile::tempdir().unwrap();
    write(&dir.path().join("a.rs"), b"fn a() {}\n");
    let dump_path = dir.path().join("dump.txt");
    let dump = fs::File::create(&dump_path).unwrap();
    let out = run(pulp()
        .args(["-o", "/dev/stdout"])
        .arg(dir.path())
        .stdout(Stdio::from(dump)));
    assert!(out.status.success(), "{}", stderr(&out));
    let text = fs::read_to_string(&dump_path).unwrap();
    assert!(text.contains("FILE: a.rs"), "{text}");
    assert!(!text.contains("dump.txt"), "{text}");
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

#[test]
fn test_pulp_list_with_binary_file_returns_only_dumped_paths() {
    let dir = tempfile::tempdir().unwrap();
    write(&dir.path().join("a.rs"), b"fn a() {}\n");
    write(
        &dir.path().join("logo.png"),
        &[0x89, b'P', b'N', b'G', 0, 0],
    );
    let out = run(pulp().arg("--list").arg(dir.path()));
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "a.rs\n");
    assert!(
        stderr(&out).starts_with("listed 1 files in "),
        "{}",
        stderr(&out)
    );
    let out = run(pulp().args(["--list", "--binaries"]).arg(dir.path()));
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "a.rs\nlogo.png\n");
    assert!(
        stderr(&out).starts_with("listed 2 files in "),
        "{}",
        stderr(&out)
    );
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
