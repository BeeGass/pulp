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

/// A page the parse model finds shallow, but whose unclosed paragraphs put
/// the plain count of open tags far past the depth limit.
fn page_of_unclosed_paragraphs() -> String {
    format!(
        "<!doctype html><h1>Tide Tables</h1>{}",
        "<p>low water".repeat(300)
    )
}

#[test]
fn test_pulp_cli_isolated_with_html_the_plain_count_doubts_renders_it_in_a_child() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("site");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("page.html"), page_of_unclosed_paragraphs()).unwrap();

    // With no time to run, the child fails and the page is stripped: the
    // heading loses the `#` html2text gives it.
    let (_, stripped) = pack_isolated(&root, &dir.path().join("zero.txt"), Some("0"));
    assert!(stripped.contains("\nTide Tables\n"), "{stripped}");
    assert!(!stripped.contains("# Tide Tables"), "{stripped}");

    // Given time, the child renders the page as this process would. A child
    // that isolated the page again would recurse until the parent's timeout.
    let (_, rendered) = pack_isolated(&root, &dir.path().join("dump.txt"), None);
    let in_process = Command::new(env!("CARGO_BIN_EXE_pulp"))
        .env("PULP_ISOLATE", "0")
        .arg(&root)
        .output()
        .expect("spawn pulp");
    assert!(in_process.status.success(), "{in_process:?}");
    let in_process = String::from_utf8(in_process.stdout).unwrap();
    assert!(rendered.contains("# Tide Tables"), "{rendered}");
    assert_eq!(rendered, in_process);
}

/// A PDF whose one font names a CMap the PDF parser has no table for
/// (`90ms-RKSJ-H`), which makes that parser panic.
fn pdf_that_panics() -> Vec<u8> {
    let objects = [
        "<< /Type /Catalog /Pages 2 0 R >>",
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] \
         /Resources << /Font << /F1 4 0 R >> >> /Contents 6 0 R >>",
        "<< /Type /Font /Subtype /Type0 /BaseFont /KozMinPro-Regular \
         /Encoding /90ms-RKSJ-H /DescendantFonts [5 0 R] >>",
        "<< /Type /Font /Subtype /CIDFontType0 /BaseFont /KozMinPro-Regular \
         /CIDSystemInfo << /Registry (Adobe) /Ordering (Japan1) /Supplement 4 >> >>",
    ];
    let content = "BT /F1 12 Tf 10 100 Td <0041> Tj ET";
    let mut pdf = String::from("%PDF-1.4\n");
    let mut offsets = Vec::new();
    for (i, body) in objects.iter().enumerate() {
        offsets.push(pdf.len());
        pdf.push_str(&format!("{} 0 obj\n{body}\nendobj\n", i + 1));
    }
    offsets.push(pdf.len());
    pdf.push_str(&format!(
        "6 0 obj\n<< /Length {} >>\nstream\n{content}\nendstream\nendobj\n",
        content.len()
    ));
    let xref = pdf.len();
    pdf.push_str("xref\n0 7\n0000000000 65535 f \n");
    for offset in offsets {
        pdf.push_str(&format!("{offset:010} 00000 n \n"));
    }
    pdf.push_str(&format!(
        "trailer\n<< /Size 7 /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n"
    ));
    pdf.into_bytes()
}

const PANIC_NOTE: &str = "extractor panicked: unsupported encoding 90ms-RKSJ-H";

#[test]
fn test_pulp_extract_child_with_panicking_parser_prints_only_the_note() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_pulp"))
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
        .write_all(&pdf_that_panics())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(!output.status.success(), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    // No thread name or id, no source location, no backtrace hint.
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        format!("{PANIC_NOTE}\n")
    );
}

#[test]
fn test_pulp_cli_with_panicking_parser_writes_the_same_note_isolated_or_not() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("docs");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("cjk.pdf"), pdf_that_panics()).unwrap();
    let (_, isolated) = pack_isolated(&root, &dir.path().join("dump.txt"), None);
    let expected = format!("[error extracting cjk.pdf: {PANIC_NOTE}]");
    assert!(isolated.contains(&expected), "{isolated}");
    let in_process = Command::new(env!("CARGO_BIN_EXE_pulp"))
        .env("PULP_ISOLATE", "0")
        .arg(&root)
        .output()
        .expect("spawn pulp");
    assert!(in_process.status.success(), "{in_process:?}");
    assert_eq!(String::from_utf8(in_process.stdout).unwrap(), isolated);
}
