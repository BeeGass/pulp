//! The `pulp` binary end to end: output routing, exit codes, and dumps that
//! must stay byte-identical across runs.

use std::fs;
use std::path::Path;
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
