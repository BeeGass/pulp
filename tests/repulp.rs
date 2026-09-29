//! Re-pulps through the extraction cache: what a pack reuses from the one
//! before it, and what it reads again.

use std::fs;
use std::io::{Cursor, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, SystemTime};

use pulp::{
    ExtractCache, FileStatus, Options, OutputFormat, Packed, TreeMode, pack_manifest,
    pack_manifest_cached, scan_manifest,
};

/// An hour before now: old enough for a cache to trust the time.
fn an_hour_ago() -> SystemTime {
    SystemTime::now() - Duration::from_secs(3600)
}

fn set_mtime(path: &Path, when: SystemTime) {
    fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(when)
        .unwrap();
}

/// Write `body` to `path`, dated an hour ago.
fn write_old(path: &Path, body: &[u8]) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, body).unwrap();
    set_mtime(path, an_hour_ago());
}

/// Overwrite `path` with `body` of the same size, then put its modification
/// time back, so that only reading it could tell. A pack that shows the old
/// text reused it.
fn swap_bytes_unseen(path: &Path, body: &[u8]) {
    let meta = fs::metadata(path).unwrap();
    assert_eq!(meta.len(), body.len() as u64, "the swap must keep the size");
    fs::write(path, body).unwrap();
    set_mtime(path, meta.modified().unwrap());
}

/// A zip of `members`, stored uncompressed.
fn zip_of(members: &[(&str, &[u8])]) -> Vec<u8> {
    let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let stored =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    for (name, body) in members {
        zip.start_file(*name, stored).unwrap();
        zip.write_all(body).unwrap();
    }
    zip.finish().unwrap().into_inner()
}

/// A `.docx` of one paragraph.
fn docx_of(text: &str) -> Vec<u8> {
    let xml = format!(
        "<?xml version=\"1.0\"?><w:document \
         xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\">\
         <w:body><w:p><w:r><w:t>{text}</w:t></w:r></w:p></w:body></w:document>"
    );
    zip_of(&[("word/document.xml", xml.as_bytes())])
}

/// A PDF of `objects`, numbered from 1, with its cross-reference table.
fn pdf_of(objects: &[String]) -> Vec<u8> {
    let mut pdf = String::from("%PDF-1.4\n");
    let mut offsets = Vec::new();
    for (i, body) in objects.iter().enumerate() {
        offsets.push(pdf.len());
        pdf.push_str(&format!("{} 0 obj\n{body}\nendobj\n", i + 1));
    }
    let xref = pdf.len();
    let size = objects.len() + 1;
    pdf.push_str(&format!("xref\n0 {size}\n0000000000 65535 f \n"));
    for offset in offsets {
        pdf.push_str(&format!("{offset:010} 00000 n \n"));
    }
    pdf.push_str(&format!(
        "trailer\n<< /Size {size} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n"
    ));
    pdf.into_bytes()
}

/// A one-page PDF that draws `text` with `font` as `/F1`.
fn pdf_page(text: &str, font: &str) -> Vec<u8> {
    let stream = format!("BT /F1 12 Tf 72 720 Td ({text}) Tj ET");
    pdf_of(&[
        "<< /Type /Catalog /Pages 2 0 R >>".into(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".into(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 4 0 R \
         /Resources << /Font << /F1 5 0 R >> >> >>"
            .into(),
        format!(
            "<< /Length {} >>\nstream\n{stream}\nendstream",
            stream.len() + 1
        ),
        font.into(),
    ])
}

fn pdf_with_text(text: &str) -> Vec<u8> {
    pdf_page(
        text,
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>",
    )
}

/// A PDF whose font names a CMap the parser has no table for, so the
/// parser panics and the file comes out as an extraction error.
fn pdf_that_fails() -> Vec<u8> {
    pdf_page(
        "A",
        "<< /Type /Font /Subtype /Type0 /BaseFont /KozMinPro-Regular /Encoding /90ms-RKSJ-H \
         /DescendantFonts [<< /Type /Font /Subtype /CIDFontType0 /BaseFont /KozMinPro-Regular \
         /CIDSystemInfo << /Registry (Adobe) /Ordering (Japan1) /Supplement 4 >> >>] >>",
    )
}

/// A folder holding a file for each outcome a pack gives, dated an hour ago.
fn mixed_folder(root: &Path) {
    write_old(&root.join("src/lib.rs"), b"pub fn tide() -> u32 { 42 }\n");
    write_old(&root.join("notes.md"), b"# Tides\n\nNorth pier.\n");
    write_old(
        &root.join("page.html"),
        b"<html><body><h1>Gauge</h1><p>Low &amp; high</p></body></html>",
    );
    write_old(&root.join("data.csv"), b"t,height\n0,1.2\n1,1.4\n");
    write_old(
        &root.join("bundle.zip"),
        &zip_of(&[
            ("a.txt", b"alpha\n"),
            ("dir/b.txt", b"bravo\n"),
            (".env", b"SECRET=1\n"),
        ]),
    );
    write_old(&root.join("paper.pdf"), &pdf_with_text("Tide tables"));
    write_old(&root.join("broken.pdf"), b"%PDF-1.4 garbage");
    write_old(
        &root.join("report.pdf"),
        b"<html><head><title>Preparing to download ...</title></head></html>",
    );
    write_old(&root.join("memo.docx"), &docx_of("Board minutes"));
    write_old(&root.join("pic.png"), b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR");
    write_old(&root.join("big.log"), &[b'x'; 5000]);
    write_old(&root.join("empty.txt"), b"");
}

fn options_for(root: &Path) -> Options {
    Options {
        roots: vec![root.to_path_buf()],
        gitignore: false,
        follow_archives: true,
        max_file_size: 4096,
        ..Options::default()
    }
}

fn render(packed: &Packed, opts: &Options, format: OutputFormat) -> Vec<u8> {
    let opts = Options {
        format,
        ..opts.clone()
    };
    let mut out = Vec::new();
    pulp::render::write_all(&mut out, packed, &opts).unwrap();
    out
}

/// Every total of a pack but the time it took.
fn totals(packed: &Packed) -> (usize, usize, u64, usize, usize, bool, bool) {
    let stats = &packed.stats;
    (
        stats.files_extracted,
        stats.files_skipped,
        stats.bytes_read,
        stats.chars_emitted,
        stats.tokens_est,
        stats.truncated,
        stats.cancelled,
    )
}

fn cached(opts: &Options, cache: Option<&ExtractCache>) -> ExtractCache {
    let manifest = scan_manifest(opts).unwrap();
    pack_manifest_cached(&manifest, opts, None, None, None, cache).unwrap()
}

fn uncached(opts: &Options) -> Packed {
    let manifest = scan_manifest(opts).unwrap();
    pack_manifest(&manifest, opts, None, None, None).unwrap()
}

/// The text a pack gave the file at `relative`.
fn text_of<'a>(packed: &'a Packed, relative: &str) -> &'a str {
    &packed
        .files
        .iter()
        .find(|file| file.relative == relative)
        .unwrap_or_else(|| panic!("no {relative} in {:?}", packed.files))
        .text
}

#[test]
fn test_pack_manifest_cached_with_unchanged_files_returns_the_uncached_dump() {
    let dir = tempfile::tempdir().unwrap();
    mixed_folder(dir.path());
    let opts = options_for(dir.path());
    let manifest = scan_manifest(&opts).unwrap();
    let plain = pack_manifest(&manifest, &opts, None, None, None).unwrap();
    let first = pack_manifest_cached(&manifest, &opts, None, None, None, None).unwrap();
    assert_eq!(first.reused(), 0);
    assert_eq!(first.len(), manifest.entries.len());

    let done = AtomicUsize::new(0);
    let second =
        pack_manifest_cached(&manifest, &opts, None, Some(&done), None, Some(&first)).unwrap();
    assert_eq!(second.reused(), manifest.entries.len());
    assert_eq!(done.load(Ordering::SeqCst), manifest.entries.len());
    let statuses: Vec<&str> = plain.files.iter().map(|f| f.status.as_str()).collect();
    for status in ["extracted", "unreadable", "skipped_binary", "too_large"] {
        assert!(statuses.contains(&status), "no {status} in {statuses:?}");
    }
    for pack in [first.packed(), second.packed()] {
        assert_eq!(pack.files, plain.files);
        assert_eq!(pack.tree, plain.tree);
        assert_eq!(totals(pack), totals(&plain));
        for format in [
            OutputFormat::Plain,
            OutputFormat::Markdown,
            OutputFormat::Xml,
        ] {
            assert_eq!(
                render(pack, &opts, format),
                render(&plain, &opts, format),
                "{format:?}"
            );
        }
    }
}

#[test]
fn test_pack_manifest_cached_with_one_edited_file_rereads_only_that_file() {
    let dir = tempfile::tempdir().unwrap();
    for name in ["a.txt", "b.txt", "c.txt"] {
        write_old(
            &dir.path().join(name),
            format!("{name} as written\n").as_bytes(),
        );
    }
    let opts = options_for(dir.path());
    let first = cached(&opts, None);

    fs::write(dir.path().join("a.txt"), "a.txt, edited since\n").unwrap();
    swap_bytes_unseen(&dir.path().join("b.txt"), b"B.TXT AS WRITTEN\n");
    let second = cached(&opts, Some(&first));

    assert_eq!(second.reused(), 2);
    let packed = second.packed();
    assert_eq!(text_of(packed, "a.txt"), "a.txt, edited since\n");
    // Its size and time are unchanged, so its old text was reused unread.
    assert_eq!(text_of(packed, "b.txt"), "b.txt as written\n");
    assert_eq!(text_of(packed, "c.txt"), "c.txt as written\n");
    assert!(
        packed
            .files
            .iter()
            .all(|file| file.status == FileStatus::Extracted),
        "{:?}",
        packed.files
    );
}

#[test]
fn test_pack_manifest_cached_with_changed_extraction_settings_reuses_nothing() {
    let dir = tempfile::tempdir().unwrap();
    mixed_folder(dir.path());
    write_old(
        &dir.path().join("lab.ipynb"),
        br#"{"cells": [{"cell_type": "code", "source": ["1 + 1"], "outputs": [{"output_type": "execute_result", "data": {"text/plain": ["2"]}}]}]}"#,
    );
    write_old(&dir.path().join(".hidden.txt"), b"hidden\n");
    let other_root = tempfile::tempdir().unwrap();
    mixed_folder(other_root.path());
    let opts = options_for(dir.path());
    let first = cached(&opts, None);

    let changes = [
        Options {
            source_mode: true,
            ..opts.clone()
        },
        Options {
            notebook_outputs: true,
            ..opts.clone()
        },
        Options {
            follow_archives: false,
            ..opts.clone()
        },
        Options {
            skip_binaries: false,
            ..opts.clone()
        },
        Options {
            max_file_size: 8192,
            ..opts.clone()
        },
        Options {
            hidden: true,
            ..opts.clone()
        },
        Options {
            exclude: vec!["*.md".into()],
            ..opts.clone()
        },
        Options {
            include: vec!["**/*.txt".into()],
            ..opts.clone()
        },
        Options {
            default_excludes: false,
            ..opts.clone()
        },
        Options {
            follow_links: true,
            ..opts.clone()
        },
        Options {
            roots: vec![other_root.path().to_path_buf()],
            ..opts.clone()
        },
    ];
    for changed in changes {
        assert!(!first.is_valid_for(&changed));
        let again = cached(&changed, Some(&first));
        assert_eq!(again.reused(), 0, "{changed:?}");
        let fresh = uncached(&changed);
        assert_eq!(again.packed().files, fresh.files, "{changed:?}");
    }

    // How the dump is drawn needs no new extraction.
    let redrawn = Options {
        format: OutputFormat::Markdown,
        tree: TreeMode::Full,
        ..opts.clone()
    };
    assert!(first.is_valid_for(&redrawn));
    let again = cached(&redrawn, Some(&first));
    assert_eq!(again.reused(), first.len());
    let fresh = uncached(&redrawn);
    assert_eq!(again.packed().tree, fresh.tree);
    assert_eq!(totals(again.packed()), totals(&fresh));
}

#[test]
fn test_pack_manifest_cached_with_expanded_archive_returns_its_members_from_the_cache() {
    let dir = tempfile::tempdir().unwrap();
    let zip = dir.path().join("bundle.zip");
    write_old(
        &zip,
        &zip_of(&[
            ("a.txt", b"alpha\n"),
            ("dir/b.txt", b"bravo\n"),
            (".env", b"SECRET=1\n"),
        ]),
    );
    let opts = options_for(dir.path());
    let first = cached(&opts, None);
    let entry = first.entry("bundle.zip").expect("the archive is cached");
    assert_eq!(entry.size, fs::metadata(&zip).unwrap().len());
    assert_eq!(entry.bytes_read(), entry.size);
    assert!(!entry.archive_cut());
    let members: Vec<(&str, &str)> = first
        .entry_files("bundle.zip")
        .map(|file| (file.relative.as_str(), file.text.as_str()))
        .collect();
    assert_eq!(
        members,
        [
            ("bundle.zip/a.txt", "alpha\n"),
            ("bundle.zip/dir/b.txt", "bravo\n")
        ]
    );

    // The same size and time, other members: only a read would see them.
    swap_bytes_unseen(
        &zip,
        &zip_of(&[
            ("a.txt", b"ALPHA\n"),
            ("dir/b.txt", b"BRAVO\n"),
            (".env", b"SECRET=2\n"),
        ]),
    );
    let second = cached(&opts, Some(&first));
    assert_eq!(second.reused(), 1);
    assert_eq!(second.packed().files, first.packed().files);
    assert_eq!(totals(second.packed()), totals(first.packed()));
    assert_eq!(
        second.entry_files("bundle.zip").count(),
        2,
        "the reused members are cached again"
    );
}

#[test]
fn test_pack_manifest_cached_with_file_modified_just_before_the_pack_rereads_it() {
    let dir = tempfile::tempdir().unwrap();
    write_old(&dir.path().join("old.txt"), b"old\n");
    fs::write(dir.path().join("new.txt"), b"new\n").unwrap();
    let opts = options_for(dir.path());
    let first = cached(&opts, None);
    // Its time may not yet tell a second edit within the same tick.
    assert!(first.entry("new.txt").is_none());
    assert!(first.entry("old.txt").is_some());
    let second = cached(&opts, Some(&first));
    assert_eq!(second.reused(), 1);
}

#[test]
fn test_pack_manifest_cached_with_failed_extraction_rereads_it() {
    let dir = tempfile::tempdir().unwrap();
    write_old(&dir.path().join("cjk.pdf"), &pdf_that_fails());
    write_old(&dir.path().join("ok.txt"), b"fine\n");
    let opts = options_for(dir.path());
    let first = cached(&opts, None);
    let failed = first
        .packed()
        .files
        .iter()
        .find(|file| file.relative == "cjk.pdf")
        .unwrap();
    assert!(
        matches!(failed.status, FileStatus::Error(_)),
        "{:?}",
        failed.status
    );
    assert!(first.entry("cjk.pdf").is_none());
    let second = cached(&opts, Some(&first));
    assert_eq!(second.reused(), 1);
    assert_eq!(second.packed().files, first.packed().files);
}

#[test]
fn test_pack_manifest_cached_with_truncated_or_cancelled_pack_returns_the_uncached_totals() {
    let dir = tempfile::tempdir().unwrap();
    mixed_folder(dir.path());
    let opts = Options {
        max_entries: 5,
        ..options_for(dir.path())
    };
    let first = cached(&opts, None);
    let second = cached(&opts, Some(&first));
    assert_eq!(second.reused(), 5);
    assert!(second.packed().stats.truncated);
    assert_eq!(totals(second.packed()), totals(&uncached(&opts)));

    // A pack cancelled before it began keeps nothing, and the next pack
    // reads every file.
    let opts = options_for(dir.path());
    let manifest = scan_manifest(&opts).unwrap();
    let cancel = AtomicBool::new(true);
    let stopped =
        pack_manifest_cached(&manifest, &opts, Some(&cancel), None, None, Some(&first)).unwrap();
    assert!(stopped.packed().files.is_empty());
    assert!(stopped.packed().stats.cancelled && stopped.packed().stats.truncated);
    assert!(stopped.is_empty());
    let resumed = pack_manifest_cached(&manifest, &opts, None, None, None, Some(&stopped)).unwrap();
    assert_eq!(resumed.reused(), 0);
    assert_eq!(resumed.packed().files, uncached(&opts).files);
}

#[cfg(unix)]
#[test]
fn test_pack_manifest_cached_with_cached_file_swapped_for_symlink_returns_changed() {
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    write_old(&dir.path().join("a.txt"), b"inside!\n");
    write_old(&dir.path().join("sub/b.txt"), b"inside!\n");
    write_old(&outside.path().join("a.txt"), b"outside\n");
    write_old(&outside.path().join("sub/b.txt"), b"outside\n");
    let opts = options_for(dir.path());
    let manifest = scan_manifest(&opts).unwrap();
    let first = pack_manifest_cached(&manifest, &opts, None, None, None, None).unwrap();
    assert_eq!(first.len(), 2);

    // Links to files of the same size and time: only the path tells.
    let when = fs::metadata(dir.path().join("a.txt"))
        .unwrap()
        .modified()
        .unwrap();
    set_mtime(&outside.path().join("a.txt"), when);
    set_mtime(&outside.path().join("sub/b.txt"), when);
    fs::remove_file(dir.path().join("a.txt")).unwrap();
    std::os::unix::fs::symlink(outside.path().join("a.txt"), dir.path().join("a.txt")).unwrap();
    fs::rename(dir.path().join("sub"), dir.path().join("sub.old")).unwrap();
    std::os::unix::fs::symlink(outside.path().join("sub"), dir.path().join("sub")).unwrap();

    let second = pack_manifest_cached(&manifest, &opts, None, None, None, Some(&first)).unwrap();
    assert_eq!(second.reused(), 0);
    for file in &second.packed().files {
        assert_eq!(file.status, FileStatus::Changed, "{file:?}");
        assert!(file.text.contains("replaced by a symlink"), "{file:?}");
    }
    assert!(second.is_empty());
}
