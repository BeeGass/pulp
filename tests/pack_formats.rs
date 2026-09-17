use std::fs;
use std::io::{Cursor, Write};
use std::path::PathBuf;

use pulp::config::default_exclude_globs;
use pulp::{Kind, Options, OutputFormat, TreeMode, classify, pack};
use tempfile::tempdir;
use zip::ZipWriter;
use zip::write::SimpleFileOptions;

fn opts(root: PathBuf) -> Options {
    Options {
        roots: vec![root],
        gitignore: false,
        ..Options::default()
    }
}

#[test]
fn test_pack_with_rust_file_returns_extracted_source() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("lib.rs"), "pub fn n() -> u8 { 1 }\n").unwrap();
    let packed = pack(&opts(dir.path().to_path_buf())).unwrap();
    let file = packed
        .files
        .iter()
        .find(|f| f.relative == "lib.rs")
        .expect("lib.rs");
    assert_eq!(file.kind, Kind::Text);
    assert!(file.text.contains("pub fn n"));
}

#[test]
fn test_pack_with_lean_file_returns_extracted_source() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("Basic.lean"), "def n : Nat := 0\n").unwrap();
    let packed = pack(&opts(dir.path().to_path_buf())).unwrap();
    let file = packed
        .files
        .iter()
        .find(|f| f.relative == "Basic.lean")
        .expect("Basic.lean");
    assert_eq!(file.kind, Kind::Text);
    assert!(file.text.contains("def n"));
}

#[test]
fn test_pack_with_npz_file_returns_extracted_not_skipped_binary() {
    let dir = tempdir().unwrap();
    let npz = tiny_npz();
    fs::write(dir.path().join("params.npz"), npz).unwrap();
    let packed = pack(&opts(dir.path().to_path_buf())).unwrap();
    let file = packed
        .files
        .iter()
        .find(|f| f.relative == "params.npz")
        .expect("params.npz");
    assert_eq!(file.kind, Kind::Npz);
    assert_ne!(file.status, pulp::FileStatus::SkippedBinary);
    assert!(
        file.text.contains("arr.npy") || file.text.contains("shape"),
        "{}",
        file.text
    );
}

#[test]
fn test_pack_with_node_modules_returns_without_vendor_js() {
    let dir = tempdir().unwrap();
    fs::create_dir_all(dir.path().join("node_modules")).unwrap();
    fs::write(dir.path().join("node_modules/skip.js"), "x").unwrap();
    fs::write(dir.path().join("keep.rs"), "x").unwrap();
    let packed = pack(&opts(dir.path().to_path_buf())).unwrap();
    assert!(packed.files.iter().any(|f| f.relative == "keep.rs"));
    assert!(
        !packed
            .files
            .iter()
            .any(|f| f.relative.contains("node_modules"))
    );
}

#[test]
fn test_pack_with_markdown_format_returns_headings() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("a.rs"), "fn a() {}\n").unwrap();
    let o = Options {
        format: OutputFormat::Markdown,
        ..opts(dir.path().to_path_buf())
    };
    let packed = pack(&o).unwrap();
    let mut buf = Vec::new();
    pulp::render::write_all(&mut buf, &packed, &o).unwrap();
    let s = String::from_utf8(buf).unwrap();
    assert!(s.contains("## a.rs"));
    assert!(s.contains("```"));
}

#[test]
fn test_pack_with_xml_format_returns_documents() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("a.rs"), "fn a() {}\n").unwrap();
    let o = Options {
        format: OutputFormat::Xml,
        tree: TreeMode::Selected,
        ..opts(dir.path().to_path_buf())
    };
    let packed = pack(&o).unwrap();
    let mut buf = Vec::new();
    pulp::render::write_all(&mut buf, &packed, &o).unwrap();
    let s = String::from_utf8(buf).unwrap();
    assert!(s.contains("<documents>"));
    assert!(s.contains("<document_content>"));
}

#[test]
fn test_pack_with_plain_format_returns_file_headers() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("a.rs"), "fn a() {}\n").unwrap();
    let o = Options {
        format: OutputFormat::Plain,
        ..opts(dir.path().to_path_buf())
    };
    let packed = pack(&o).unwrap();
    let mut buf = Vec::new();
    pulp::render::write_all(&mut buf, &packed, &o).unwrap();
    let s = String::from_utf8(buf).unwrap();
    assert!(s.contains("FILE: a.rs"));
}

#[test]
fn test_from_path_with_dump_md_returns_markdown() {
    assert_eq!(
        OutputFormat::from_path(std::path::Path::new("dump.md")),
        Some(OutputFormat::Markdown)
    );
    assert_eq!(
        OutputFormat::from_path(std::path::Path::new("dump.txt")),
        Some(OutputFormat::Plain)
    );
    assert_eq!(
        OutputFormat::from_path(std::path::Path::new("dump.xml")),
        Some(OutputFormat::Xml)
    );
}

#[test]
fn test_classify_with_rs_lean_npz_returns_expected_kinds() {
    assert_eq!(
        classify(std::path::Path::new("src/main.rs"), None),
        Kind::Text
    );
    assert_eq!(
        classify(std::path::Path::new("Dual.lean"), None),
        Kind::Text
    );
    assert_eq!(classify(std::path::Path::new("w.npz"), None), Kind::Npz);
}

#[test]
fn test_default_exclude_globs_does_not_drop_rs_lean_npz() {
    let g = default_exclude_globs().join(" ");
    assert!(!g.contains("*.rs"));
    assert!(!g.contains("*.lean"));
    assert!(!g.contains("*.npz"));
}

fn tiny_npz() -> Vec<u8> {
    let npy = tiny_f32_npy();
    let mut buf = Cursor::new(Vec::new());
    {
        let mut zw = ZipWriter::new(&mut buf);
        zw.start_file("arr.npy", SimpleFileOptions::default())
            .unwrap();
        zw.write_all(&npy).unwrap();
        zw.finish().unwrap();
    }
    buf.into_inner()
}

fn tiny_f32_npy() -> Vec<u8> {
    let mut header = "{'descr': '<f4', 'fortran_order': False, 'shape': (3,), }".to_string();
    let prefix = 10;
    while (prefix + header.len()) % 16 != 0 {
        header.push(' ');
    }
    header.pop();
    header.push('\n');
    let hlen = header.len() as u16;
    let mut out = Vec::new();
    out.extend_from_slice(b"\x93NUMPY");
    out.extend_from_slice(&[1u8, 0]);
    out.extend_from_slice(&hlen.to_le_bytes());
    out.extend_from_slice(header.as_bytes());
    for v in [1.0f32, 2.0, 3.0] {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}
