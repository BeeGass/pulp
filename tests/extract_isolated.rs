//! Extraction in the child `pulp` process, driven through the real binary.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::Duration;

use pulp::extract::isolate::wait_with_drain;

/// Run `cmd` to completion, failing the test instead of hanging when it
/// takes longer than `limit`.
fn output_within(cmd: &mut Command, limit: Duration) -> Output {
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn pulp");
    let (stdout, stderr, status) = wait_with_drain(&mut child, limit, 64 << 20, 1 << 20)
        .unwrap_or_else(|err| panic!("pulp did not finish: {err}"));
    Output {
        status,
        stdout,
        stderr,
    }
}

/// A `.docx` holding `paragraphs`, stored uncompressed so its size on disk
/// follows its text.
fn docx(paragraphs: &[String]) -> Vec<u8> {
    let mut xml = String::from(
        "<?xml version=\"1.0\"?><w:document \
         xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\"><w:body>",
    );
    for paragraph in paragraphs {
        xml.push_str("<w:p><w:r><w:t>");
        xml.push_str(paragraph);
        xml.push_str("</w:t></w:r></w:p>");
    }
    xml.push_str("</w:body></w:document>");
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let stored =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    zip.start_file("word/document.xml", stored).unwrap();
    zip.write_all(xml.as_bytes()).unwrap();
    zip.finish().unwrap().into_inner()
}

fn pack_isolated(root: &Path, out: &Path, timeout_ms: Option<&str>) -> (Output, String) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_pulp"));
    cmd.env("PULP_ISOLATE", "1")
        .args(["-o", out.to_str().unwrap(), root.to_str().unwrap()]);
    match timeout_ms {
        Some(ms) => cmd.env("PULP_EXTRACT_TIMEOUT_MS", ms),
        None => cmd.env_remove("PULP_EXTRACT_TIMEOUT_MS"),
    };
    let output = output_within(&mut cmd, Duration::from_secs(120));
    assert!(output.status.success(), "{output:?}");
    let dump = std::fs::read_to_string(out).unwrap();
    (output, dump)
}

#[test]
fn test_pulp_cli_isolated_with_large_docx_round_trips_through_pipes() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("docs");
    std::fs::create_dir(&root).unwrap();
    // Several MiB each way, far past any pipe buffer: a parent that wrote
    // all of stdin before reading stdout would deadlock with the child.
    let paragraphs: Vec<String> = (0..7100)
        .map(|i| format!("paragraph {i:05} {}", "tide ".repeat(198)))
        .collect();
    let bytes = docx(&paragraphs);
    assert!(bytes.len() > 7 << 20, "{} bytes", bytes.len());
    std::fs::write(root.join("almanac.docx"), &bytes).unwrap();
    let (_, dump) = pack_isolated(&root, &dir.path().join("dump.txt"), None);
    assert!(dump.len() > 6_500_000, "{} bytes of dump", dump.len());
    for i in [0, 3550, 7099] {
        assert!(dump.contains(&format!("paragraph {i:05} tide")), "{i}");
    }
}
