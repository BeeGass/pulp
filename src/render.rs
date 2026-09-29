use std::io::{self, Write};
use std::path::Path;

use crate::Packed;
use crate::config::{Options, OutputFormat};
use crate::pack::{FileStatus, PackedFile};
use crate::tree::display_path;

/// Write the packed dump in plain, markdown, or XML.
///
/// Every file gets a section except skipped binaries: their bytes and their
/// placeholder stay out of the dump unless binaries are enabled, in which
/// case the pack marks them extracted with a one-line placeholder.
pub fn write_all<W: Write>(w: &mut W, packed: &Packed, opts: &Options) -> io::Result<()> {
    match opts.format {
        OutputFormat::Plain => write_plain(w, packed, opts),
        OutputFormat::Markdown => write_markdown(w, packed, opts),
        OutputFormat::Xml => write_xml(w, packed),
    }
}

/// Directory map in the dump format, with no file bodies.
pub fn format_directory_map(
    root_label: &str,
    paths: &[String],
    format: OutputFormat,
) -> io::Result<String> {
    let packed = Packed {
        files: Vec::new(),
        tree: crate::tree::render_tree(root_label, paths),
        stats: crate::Stats::default(),
    };
    let opts = Options {
        format,
        ..Options::default()
    };
    let mut buf = Vec::new();
    write_tree(&mut buf, &packed, &opts)?;
    String::from_utf8(buf).map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
}

/// Write only the directory map in the selected dump format.
pub fn write_tree<W: Write>(w: &mut W, packed: &Packed, opts: &Options) -> io::Result<()> {
    if packed.tree.is_empty() {
        return Ok(());
    }
    match opts.format {
        OutputFormat::Plain => {
            writeln!(w, "Directory structure:")?;
            write_tree_body(w, packed)?;
        }
        OutputFormat::Markdown => {
            writeln!(w, "# Directory structure")?;
            writeln!(w)?;
            writeln!(w, "```")?;
            write_tree_body(w, packed)?;
            writeln!(w, "```")?;
        }
        OutputFormat::Xml => {
            writeln!(w, "<document_tree>")?;
            write_xml_escaped(w, &packed.tree)?;
            writeln!(w)?;
            writeln!(w, "</document_tree>")?;
        }
    }
    Ok(())
}

fn write_tree_body<W: Write>(w: &mut W, packed: &Packed) -> io::Result<()> {
    write!(w, "{}", packed.tree)?;
    if !packed.tree.ends_with('\n') {
        writeln!(w)?;
    }
    Ok(())
}

/// Files that get a section in the dump body.
fn dumped_files(packed: &Packed) -> impl Iterator<Item = &PackedFile> {
    packed
        .files
        .iter()
        .filter(|file| file.status != FileStatus::SkippedBinary)
}

fn write_plain<W: Write>(w: &mut W, packed: &Packed, opts: &Options) -> io::Result<()> {
    if !packed.tree.is_empty() {
        write_tree(w, packed, opts)?;
        writeln!(w)?;
    }
    for file in dumped_files(packed) {
        writeln!(w, "================================================")?;
        writeln!(w, "FILE: {}", display_path(&file.relative))?;
        writeln!(w, "================================================")?;
        write!(w, "{}", file.text)?;
        if !file.text.ends_with('\n') {
            writeln!(w)?;
        }
        writeln!(w)?;
    }
    Ok(())
}

fn write_markdown<W: Write>(w: &mut W, packed: &Packed, opts: &Options) -> io::Result<()> {
    if !packed.tree.is_empty() {
        write_tree(w, packed, opts)?;
        writeln!(w)?;
    }
    for file in dumped_files(packed) {
        writeln!(w, "## {}", display_path(&file.relative))?;
        writeln!(w)?;
        let fence = fence_for(&file.text);
        let lang = fence_lang(&file.relative, file.kind, opts.source_mode);
        if lang.is_empty() {
            writeln!(w, "{fence}")?;
        } else {
            writeln!(w, "{fence}{lang}")?;
        }
        write!(w, "{}", file.text)?;
        if !file.text.ends_with('\n') {
            writeln!(w)?;
        }
        writeln!(w, "{fence}")?;
        writeln!(w)?;
    }
    Ok(())
}

fn write_xml<W: Write>(w: &mut W, packed: &Packed) -> io::Result<()> {
    writeln!(w, "<documents>")?;
    if !packed.tree.is_empty() {
        writeln!(w, "<document_tree>")?;
        write_xml_escaped(w, &packed.tree)?;
        writeln!(w)?;
        writeln!(w, "</document_tree>")?;
    }
    for (i, file) in dumped_files(packed).enumerate() {
        writeln!(w, "<document index=\"{}\">", i + 1)?;
        write!(w, "<source>")?;
        write_xml_escaped(w, &display_path(&file.relative))?;
        writeln!(w, "</source>")?;
        writeln!(w, "<document_content>")?;
        write_xml_escaped(w, &file.text)?;
        writeln!(w)?;
        writeln!(w, "</document_content>")?;
        writeln!(w, "</document>")?;
    }
    writeln!(w, "</documents>")?;
    Ok(())
}

fn fence_for(text: &str) -> String {
    let mut longest = 2usize;
    let mut run = 0usize;
    for c in text.chars() {
        if c == '`' {
            run += 1;
            if run > longest {
                longest = run;
            }
        } else {
            run = 0;
        }
    }
    "`".repeat(longest + 1)
}

fn fence_lang(path: &str, kind: crate::classify::Kind, source_mode: bool) -> &'static str {
    if !source_mode
        && matches!(
            kind,
            crate::classify::Kind::Html | crate::classify::Kind::Xml
        )
    {
        return "";
    }
    match Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "rs" => "rust",
        "lean" => "lean",
        "py" => "python",
        "md" | "markdown" => "markdown",
        "ts" => "typescript",
        "tsx" => "tsx",
        "js" => "javascript",
        "json" => "json",
        "toml" => "toml",
        "html" | "htm" => "html",
        "xml" => "xml",
        "yml" | "yaml" => "yaml",
        "sh" => "bash",
        "csv" => "csv",
        "npy" | "npz" => "text",
        _ => "",
    }
}

/// Write `s` as XML 1.0 character data.
///
/// `&`, `<`, and `>` become entities. Characters XML 1.0 cannot carry at
/// all, not even as a character reference (C0 controls other than tab, line
/// feed, and carriage return, plus U+FFFE and U+FFFF), become U+FFFD, so the
/// dump stays well-formed whatever bytes a file held.
fn write_xml_escaped<W: Write>(w: &mut W, s: &str) -> io::Result<()> {
    const REPLACEMENT: &[u8] = "\u{FFFD}".as_bytes();
    let bytes = s.as_bytes();
    let mut start = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        let b = bytes[i];
        let (esc, len): (&[u8], usize) = match b {
            b'&' => (b"&amp;", 1),
            b'<' => (b"&lt;", 1),
            b'>' => (b"&gt;", 1),
            b'\t' | b'\n' | b'\r' => {
                i += 1;
                continue;
            }
            0x00..=0x1F => (REPLACEMENT, 1),
            // U+FFFE and U+FFFF encode as EF BF BE and EF BF BF. In valid
            // UTF-8, 0xEF always starts a three-byte sequence.
            0xEF if bytes.get(i + 1) == Some(&0xBF)
                && matches!(bytes.get(i + 2), Some(0xBE | 0xBF)) =>
            {
                (REPLACEMENT, 3)
            }
            _ => {
                i += 1;
                continue;
            }
        };
        w.write_all(&bytes[start..i])?;
        w.write_all(esc)?;
        i += len;
        start = i;
    }
    w.write_all(&bytes[start..])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classify::Kind;
    use crate::config::TreeMode;
    use crate::pack::{FileStatus, PackedFile, Stats};
    use std::time::Duration;

    fn packed(format: OutputFormat) -> (Packed, Options) {
        let packed = Packed {
            tree: "src\n└── lib.rs\n".into(),
            files: vec![PackedFile {
                id: "src/lib.rs".into(),
                relative: "src/lib.rs".into(),
                kind: Kind::Text,
                size: 3,
                text: "hi\n".into(),
                status: FileStatus::Extracted,
            }],
            stats: Stats {
                files_extracted: 1,
                files_skipped: 0,
                bytes_read: 3,
                chars_emitted: 3,
                tokens_est: 1,
                elapsed: Duration::from_millis(1),
                truncated: false,
                cancelled: false,
            },
        };
        let opts = Options {
            format,
            tree: TreeMode::Selected,
            ..Options::default()
        };
        (packed, opts)
    }

    #[test]
    fn test_write_all_with_plain_returns_file_headers() {
        let (p, opts) = packed(OutputFormat::Plain);
        let mut buf = Vec::new();
        write_all(&mut buf, &p, &opts).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("FILE: src/lib.rs"));
        assert!(s.contains("Directory structure:"));
    }

    #[test]
    fn test_write_all_with_markdown_returns_fences() {
        let (p, opts) = packed(OutputFormat::Markdown);
        let mut buf = Vec::new();
        write_all(&mut buf, &p, &opts).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("## src/lib.rs"));
        assert!(s.contains("```rust"));
    }

    #[test]
    fn test_write_all_with_xml_returns_documents() {
        let (p, opts) = packed(OutputFormat::Xml);
        let mut buf = Vec::new();
        write_all(&mut buf, &p, &opts).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("<documents>"));
        assert!(s.contains("<document_content>"));
    }

    #[test]
    fn test_write_tree_with_plain_returns_directory_structure_only() {
        let (p, opts) = packed(OutputFormat::Plain);
        let mut buf = Vec::new();
        write_tree(&mut buf, &p, &opts).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("Directory structure:"));
        assert!(s.contains("lib.rs"));
        assert!(!s.contains("FILE:"));
    }

    #[test]
    fn test_write_tree_with_markdown_returns_fenced_tree() {
        let (p, opts) = packed(OutputFormat::Markdown);
        let mut buf = Vec::new();
        write_tree(&mut buf, &p, &opts).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("# Directory structure"));
        assert!(s.contains("```"));
        assert!(!s.contains("## src/lib.rs"));
    }

    #[test]
    fn test_fence_for_with_five_backticks_returns_six() {
        let text = "x\n`````\ny";
        assert_eq!(fence_for(text), "``````");
        assert_eq!(fence_for("no ticks"), "```");
        assert_eq!(fence_for("has ``` fence"), "````");
    }

    #[test]
    fn test_format_directory_map_with_each_format_omits_file_bodies() {
        let paths = ["README.md".to_string(), "src/main.rs".to_string()];
        let xml = format_directory_map("website", &paths, OutputFormat::Xml).unwrap();
        assert!(xml.starts_with("<document_tree>\n"));
        assert!(xml.contains("website/\n"));
        assert!(xml.contains("└── src/\n"));
        assert!(xml.trim_end().ends_with("</document_tree>"));
        assert!(!xml.contains("<documents>"));
        assert!(!xml.contains("<document_content>"));
        assert!(!xml.contains("FILE:"));

        let md = format_directory_map("website", &paths, OutputFormat::Markdown).unwrap();
        assert!(md.starts_with("# Directory structure\n"));
        assert!(md.contains("```\nwebsite/\n"));
        assert!(!md.contains("## README.md"));

        let plain = format_directory_map("website", &paths, OutputFormat::Plain).unwrap();
        assert!(plain.starts_with("Directory structure:\nwebsite/\n"));
        assert!(!plain.contains("FILE:"));
    }

    fn file(relative: &str, text: &str, status: FileStatus) -> PackedFile {
        PackedFile {
            id: relative.into(),
            relative: relative.into(),
            kind: Kind::Text,
            size: text.len() as u64,
            text: text.into(),
            status,
        }
    }

    fn render(packed: &Packed, format: OutputFormat) -> String {
        let opts = Options {
            format,
            ..Options::default()
        };
        let mut buf = Vec::new();
        write_all(&mut buf, packed, &opts).unwrap();
        String::from_utf8(buf).unwrap()
    }

    fn is_xml_char(c: char) -> bool {
        matches!(c, '\t' | '\n' | '\r' | '\u{20}'..='\u{D7FF}' | '\u{E000}'..='\u{FFFD}')
            || c >= '\u{10000}'
    }

    #[test]
    fn test_write_all_with_xml_and_control_chars_returns_well_formed_xml() {
        let path = "odd\u{1}dir/a\nb <&> \u{FFFE}.txt";
        let text = "nul\u{0} bell\u{7} esc\u{1b} vt\u{b} ff\u{c} ]]> \u{FFFF} tab\tcr\r\n";
        let packed = Packed {
            tree: crate::tree::render_tree("root\u{2}", &[path.to_string()]),
            files: vec![file(path, text, FileStatus::Extracted)],
            stats: Stats::default(),
        };
        let xml = render(&packed, OutputFormat::Xml);
        let bad: Vec<char> = xml.chars().filter(|c| !is_xml_char(*c)).collect();
        assert!(bad.is_empty(), "forbidden XML chars {bad:?} in {xml:?}");

        use quick_xml::events::Event;
        let mut reader = quick_xml::Reader::from_str(&xml);
        let mut source = String::new();
        let mut sources = 0;
        let mut in_source = false;
        loop {
            match reader.read_event() {
                Ok(Event::Start(e)) => {
                    in_source = e.name().as_ref() == "source";
                    sources += usize::from(in_source);
                }
                Ok(Event::Text(t)) if in_source => source.push_str(&t),
                Ok(Event::GeneralRef(r)) if in_source => source.push_str(match r.as_ref() {
                    "lt" => "<",
                    "gt" => ">",
                    "amp" => "&",
                    other => panic!("unexpected entity {other}"),
                }),
                Ok(Event::End(_)) => in_source = false,
                Ok(Event::Eof) => break,
                Ok(_) => {}
                Err(err) => panic!("malformed XML: {err}\n{xml}"),
            }
        }
        assert_eq!(sources, 1, "{xml}");
        assert_eq!(source, "odd\\u{1}dir/a\\nb <&> \\u{fffe}.txt");
        assert!(xml.contains("tab\tcr\r\n"), "{xml:?}");
        assert!(xml.contains("]]&gt;"), "{xml:?}");
        assert!(xml.contains("nul\u{FFFD} bell\u{FFFD}"), "{xml:?}");
    }

    #[test]
    fn test_write_all_with_newline_in_path_keeps_each_header_on_one_line() {
        let packed = Packed {
            tree: String::new(),
            files: vec![file(
                "a\nFILE: forged.txt",
                "real body\n",
                FileStatus::Extracted,
            )],
            stats: Stats::default(),
        };
        let plain = render(&packed, OutputFormat::Plain);
        assert!(plain.contains("FILE: a\\nFILE: forged.txt\n"), "{plain}");
        let headers = plain.lines().filter(|l| l.starts_with("FILE: ")).count();
        assert_eq!(headers, 1, "{plain}");

        let md = render(&packed, OutputFormat::Markdown);
        assert!(md.contains("## a\\nFILE: forged.txt\n"), "{md}");
        assert!(!md.contains("\nFILE: forged"), "{md}");
    }

    #[test]
    fn test_write_all_with_skipped_binary_omits_its_section() {
        let packed = Packed {
            tree: String::new(),
            files: vec![
                file("a.rs", "fn a() {}\n", FileStatus::Extracted),
                file(
                    "logo.png",
                    "[binary file, 4 bytes]",
                    FileStatus::SkippedBinary,
                ),
                file("big.log", "[too large]", FileStatus::TooLarge(1)),
            ],
            stats: Stats::default(),
        };
        for format in [
            OutputFormat::Plain,
            OutputFormat::Markdown,
            OutputFormat::Xml,
        ] {
            let dump = render(&packed, format);
            assert!(!dump.contains("logo.png"), "{format:?}: {dump}");
            assert!(!dump.contains("[binary file"), "{format:?}: {dump}");
            assert!(dump.contains("a.rs"), "{format:?}: {dump}");
            assert!(dump.contains("big.log"), "{format:?}: {dump}");
        }
        let xml = render(&packed, OutputFormat::Xml);
        assert!(xml.contains("<document index=\"2\">"), "{xml}");
        assert!(!xml.contains("<document index=\"3\">"), "{xml}");
    }

    #[test]
    fn test_write_tree_with_xml_returns_document_tree() {
        let (p, opts) = packed(OutputFormat::Xml);
        let mut buf = Vec::new();
        write_tree(&mut buf, &p, &opts).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("<document_tree>"));
        assert!(!s.contains("<document_content>"));
    }
}
