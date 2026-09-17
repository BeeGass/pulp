use std::io::{self, Write};
use std::path::Path;

use crate::Packed;
use crate::config::{Options, OutputFormat};

/// Write the packed dump in plain, markdown, or XML.
pub fn write_all<W: Write>(w: &mut W, packed: &Packed, opts: &Options) -> io::Result<()> {
    match opts.format {
        OutputFormat::Plain => write_plain(w, packed, opts),
        OutputFormat::Markdown => write_markdown(w, packed, opts),
        OutputFormat::Xml => write_xml(w, packed),
    }
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
            writeln!(w, "{}", xml_escape(&packed.tree))?;
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

fn write_plain<W: Write>(w: &mut W, packed: &Packed, opts: &Options) -> io::Result<()> {
    if !packed.tree.is_empty() {
        write_tree(w, packed, opts)?;
        writeln!(w)?;
    }
    for file in &packed.files {
        writeln!(w, "================================================")?;
        writeln!(w, "FILE: {}", file.relative)?;
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
    for file in &packed.files {
        writeln!(w, "## {}", file.relative)?;
        writeln!(w)?;
        let fence = fence_for(&file.text);
        let lang = fence_lang(&file.relative);
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
        writeln!(w, "{}", xml_escape(&packed.tree))?;
        writeln!(w, "</document_tree>")?;
    }
    for (i, file) in packed.files.iter().enumerate() {
        writeln!(w, "<document index=\"{}\">", i + 1)?;
        writeln!(w, "<source>{}</source>", xml_escape(&file.relative))?;
        writeln!(w, "<document_content>")?;
        writeln!(w, "{}", xml_escape(&file.text))?;
        writeln!(w, "</document_content>")?;
        writeln!(w, "</document>")?;
    }
    writeln!(w, "</documents>")?;
    Ok(())
}

fn fence_for(text: &str) -> &'static str {
    if text.contains("````") {
        "`````"
    } else if text.contains("```") {
        "````"
    } else {
        "```"
    }
}

fn fence_lang(path: &str) -> &'static str {
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

fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(ch),
        }
    }
    out
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
    fn test_write_tree_with_xml_returns_document_tree() {
        let (p, opts) = packed(OutputFormat::Xml);
        let mut buf = Vec::new();
        write_tree(&mut buf, &p, &opts).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("<document_tree>"));
        assert!(!s.contains("<document_content>"));
    }
}
