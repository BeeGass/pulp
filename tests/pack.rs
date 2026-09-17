use std::fs;
use std::path::{Path, PathBuf};

use pulp::{FileStatus, Kind, Options, OutputFormat, pack};

fn testdata(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("testdata")
        .join(name)
}

fn options_for(root: &Path) -> Options {
    Options {
        roots: vec![root.to_path_buf()],
        gitignore: false,
        ..Options::default()
    }
}

fn find_file<'a>(packed: &'a pulp::Packed, name: &str) -> &'a pulp::PackedFile {
    packed
        .files
        .iter()
        .find(|f| f.relative == name || f.relative.ends_with(&format!("/{name}")))
        .unwrap_or_else(|| {
            let names: Vec<&str> = packed.files.iter().map(|f| f.relative.as_str()).collect();
            panic!("expected {name} in packed files, got {names:?}");
        })
}

fn dump_with(opts: &Options) -> String {
    let packed = pack(opts).unwrap_or_else(|e| panic!("pack failed: {e}"));
    let mut buf = Vec::new();
    pulp::render::write_all(&mut buf, &packed, opts)
        .unwrap_or_else(|e| panic!("render failed: {e}"));
    String::from_utf8(buf).expect("dump is utf-8")
}

fn crc32_ieee(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

fn push_u16(buf: &mut Vec<u8>, v: u16) {
    buf.extend_from_slice(&v.to_le_bytes());
}

fn push_u32(buf: &mut Vec<u8>, v: u32) {
    buf.extend_from_slice(&v.to_le_bytes());
}

/// NPY v1.0 little-endian float64 vector of length 2.
fn minimal_npy() -> Vec<u8> {
    let mut header = String::from("{'descr': '<f8', 'fortran_order': False, 'shape': (2,), }");
    let padding = 64 - ((10 + header.len() + 1) % 64);
    header.push_str(&" ".repeat(padding));
    header.push('\n');
    let hlen = u16::try_from(header.len()).expect("npy header fits u16");

    let mut out = Vec::new();
    out.extend_from_slice(b"\x93NUMPY\x01\x00");
    out.extend_from_slice(&hlen.to_le_bytes());
    out.extend_from_slice(header.as_bytes());
    out.extend_from_slice(&1.0f64.to_le_bytes());
    out.extend_from_slice(&2.0f64.to_le_bytes());
    out
}

/// Single stored (uncompressed) zip member. NPZ is a zip of `.npy` files.
fn zip_store(name: &str, data: &[u8]) -> Vec<u8> {
    debug_assert_eq!(crc32_ieee(b"123456789"), 0xCBF4_3926);

    let name_bytes = name.as_bytes();
    let crc = crc32_ieee(data);
    let size = u32::try_from(data.len()).expect("member fits u32");
    let name_len = u16::try_from(name_bytes.len()).expect("name fits u16");

    let mut buf = Vec::new();
    buf.extend_from_slice(b"PK\x03\x04");
    push_u16(&mut buf, 20);
    push_u16(&mut buf, 0);
    push_u16(&mut buf, 0);
    push_u16(&mut buf, 0);
    push_u16(&mut buf, 0);
    push_u32(&mut buf, crc);
    push_u32(&mut buf, size);
    push_u32(&mut buf, size);
    push_u16(&mut buf, name_len);
    push_u16(&mut buf, 0);
    buf.extend_from_slice(name_bytes);
    buf.extend_from_slice(data);
    let cd_offset = u32::try_from(buf.len()).expect("local region fits u32");

    buf.extend_from_slice(b"PK\x01\x02");
    push_u16(&mut buf, 20);
    push_u16(&mut buf, 20);
    push_u16(&mut buf, 0);
    push_u16(&mut buf, 0);
    push_u16(&mut buf, 0);
    push_u16(&mut buf, 0);
    push_u32(&mut buf, crc);
    push_u32(&mut buf, size);
    push_u32(&mut buf, size);
    push_u16(&mut buf, name_len);
    push_u16(&mut buf, 0);
    push_u16(&mut buf, 0);
    push_u16(&mut buf, 0);
    push_u16(&mut buf, 0);
    push_u32(&mut buf, 0);
    push_u32(&mut buf, 0);
    buf.extend_from_slice(name_bytes);
    let cd_size = u32::try_from(buf.len()).expect("cd end fits u32") - cd_offset;

    buf.extend_from_slice(b"PK\x05\x06");
    push_u16(&mut buf, 0);
    push_u16(&mut buf, 0);
    push_u16(&mut buf, 1);
    push_u16(&mut buf, 1);
    push_u32(&mut buf, cd_size);
    push_u32(&mut buf, cd_offset);
    push_u16(&mut buf, 0);
    buf
}

#[test]
fn test_pack_with_rust_source_returns_extracted() {
    let tmp = tempfile::tempdir().unwrap();
    fs::copy(testdata("hello.rs"), tmp.path().join("hello.rs")).unwrap();

    let packed = pack(&options_for(tmp.path())).unwrap_or_else(|e| panic!("{e}"));
    let file = find_file(&packed, "hello.rs");

    assert!(
        matches!(file.status, FileStatus::Extracted),
        "rust source must be extracted, status={:?}",
        file.status
    );
    assert_eq!(file.kind, Kind::Text);
    assert!(
        file.text.contains("fn hello"),
        "extracted rust text, got {:?}",
        file.text
    );
}

#[test]
fn test_pack_with_lean_source_returns_extracted() {
    let tmp = tempfile::tempdir().unwrap();
    fs::copy(testdata("Hello.lean"), tmp.path().join("Hello.lean")).unwrap();

    let packed = pack(&options_for(tmp.path())).unwrap_or_else(|e| panic!("{e}"));
    let file = find_file(&packed, "Hello.lean");

    assert!(
        matches!(file.status, FileStatus::Extracted),
        "lean source must be extracted, status={:?}",
        file.status
    );
    assert_eq!(file.kind, Kind::Text);
    assert!(
        file.text.contains("def hello"),
        "extracted lean text, got {:?}",
        file.text
    );
}

#[test]
fn test_pack_with_npz_returns_extracted_not_skipped_binary() {
    let tmp = tempfile::tempdir().unwrap();
    let npz = zip_store("arr.npy", &minimal_npy());
    fs::write(tmp.path().join("arr.npz"), npz).unwrap();

    let packed = pack(&options_for(tmp.path())).unwrap_or_else(|e| panic!("{e}"));
    let file = find_file(&packed, "arr.npz");

    assert_ne!(
        file.kind,
        Kind::Binary,
        "npz must not classify as binary media"
    );
    assert_eq!(file.kind, Kind::Npz);
    assert!(
        !matches!(file.status, FileStatus::SkippedBinary),
        "npz must not be skipped as binary, status={:?}",
        file.status
    );
    assert!(
        matches!(file.status, FileStatus::Extracted),
        "npz must be extracted, status={:?}",
        file.status
    );
}

#[test]
fn test_pack_with_default_exclude_returns_without_node_modules() {
    let tmp = tempfile::tempdir().unwrap();
    fs::create_dir_all(tmp.path().join("node_modules")).unwrap();
    fs::write(
        tmp.path().join("node_modules").join("foo.js"),
        "console.log(1);\n",
    )
    .unwrap();
    fs::write(tmp.path().join("keep.rs"), "pub fn keep() {}\n").unwrap();

    let packed = pack(&options_for(tmp.path())).unwrap_or_else(|e| panic!("{e}"));

    assert!(
        packed
            .files
            .iter()
            .all(|f| !f.relative.contains("node_modules")),
        "default excludes must drop node_modules, got {:?}",
        packed
            .files
            .iter()
            .map(|f| f.relative.as_str())
            .collect::<Vec<_>>()
    );
    let keep = find_file(&packed, "keep.rs");
    assert!(matches!(keep.status, FileStatus::Extracted));
}

#[test]
fn test_pack_with_gitignore_returns_without_ignored_files() {
    let tmp = tempfile::tempdir().unwrap();
    fs::create_dir(tmp.path().join(".git")).unwrap();
    fs::write(
        tmp.path().join(".git").join("HEAD"),
        "ref: refs/heads/main\n",
    )
    .unwrap();
    fs::write(tmp.path().join(".gitignore"), "secret.txt\n").unwrap();
    fs::write(tmp.path().join("secret.txt"), "token=1\n").unwrap();
    fs::write(tmp.path().join("visible.rs"), "pub fn visible() {}\n").unwrap();

    let mut opts = options_for(tmp.path());
    opts.gitignore = true;
    let packed = pack(&opts).unwrap_or_else(|e| panic!("{e}"));

    assert!(
        packed
            .files
            .iter()
            .all(|f| { f.relative != "secret.txt" && !f.relative.ends_with("/secret.txt") }),
        "gitignore must drop secret.txt, got {:?}",
        packed
            .files
            .iter()
            .map(|f| f.relative.as_str())
            .collect::<Vec<_>>()
    );
    let visible = find_file(&packed, "visible.rs");
    assert!(matches!(visible.status, FileStatus::Extracted));
}

#[test]
fn test_pack_with_plain_format_returns_file_headers() {
    let tmp = tempfile::tempdir().unwrap();
    fs::copy(testdata("hello.rs"), tmp.path().join("hello.rs")).unwrap();

    let mut opts = options_for(tmp.path());
    opts.format = OutputFormat::Plain;
    let dump = dump_with(&opts);

    assert!(
        dump.contains("FILE:"),
        "plain dump must contain FILE: headers:\n{dump}"
    );
    assert!(
        dump.contains("hello.rs"),
        "plain dump must name the file:\n{dump}"
    );
}

#[test]
fn test_pack_with_markdown_format_returns_headings_and_fences() {
    let tmp = tempfile::tempdir().unwrap();
    fs::copy(testdata("hello.rs"), tmp.path().join("hello.rs")).unwrap();

    let mut opts = options_for(tmp.path());
    opts.format = OutputFormat::Markdown;
    let dump = dump_with(&opts);

    assert!(
        dump.contains("## "),
        "markdown dump must contain ## headings:\n{dump}"
    );
    assert!(
        dump.contains("```"),
        "markdown dump must contain fences:\n{dump}"
    );
}

#[test]
fn test_pack_with_xml_format_returns_documents_markup() {
    let tmp = tempfile::tempdir().unwrap();
    fs::copy(testdata("hello.rs"), tmp.path().join("hello.rs")).unwrap();

    let mut opts = options_for(tmp.path());
    opts.format = OutputFormat::Xml;
    let dump = dump_with(&opts);

    assert!(
        dump.contains("<documents>"),
        "xml dump must contain <documents>:\n{dump}"
    );
    assert!(
        dump.contains("<document_content>"),
        "xml dump must contain <document_content>:\n{dump}"
    );
}

#[test]
fn test_from_path_with_dump_md_returns_markdown() {
    assert_eq!(
        OutputFormat::from_path(Path::new("dump.md")),
        Some(OutputFormat::Markdown)
    );
}
