mod archive;
mod csv;
mod epub;
mod html;
pub mod isolate;
mod json;
mod notebook;
mod npz;
mod office;
mod pdf;
mod rtf;
mod text;
mod xml;

use crate::classify::Kind;
use crate::error::Error;

/// Per-file extraction knobs threaded from [`crate::config::Options`].
#[derive(Debug, Clone)]
pub struct ExtractOpts {
    pub max_file_size: u64,
    pub notebook_outputs: bool,
    pub source_mode: bool,
}

impl ExtractOpts {
    #[must_use]
    pub fn from_options(opts: &crate::config::Options) -> Self {
        Self {
            max_file_size: opts.max_file_size,
            notebook_outputs: opts.notebook_outputs,
            source_mode: opts.source_mode,
        }
    }
}

/// Convert one file's bytes into LLM-readable text.
///
/// Archives are not expanded here; call [`expand_archive`] first.
pub fn extract(
    relative: &str,
    bytes: &[u8],
    kind: Kind,
    opts: &ExtractOpts,
) -> Result<String, Error> {
    match kind {
        Kind::Html if opts.source_mode => text::extract(bytes),
        Kind::Xml if opts.source_mode => text::extract(bytes),
        Kind::Json if opts.source_mode => text::extract(bytes),
        Kind::Html => html::extract(bytes),
        Kind::Xml => xml::extract(bytes),
        Kind::Json => json::extract(bytes),
        Kind::Csv => csv::extract(bytes, b','),
        Kind::Tsv => csv::extract(bytes, b'\t'),
        Kind::Pdf => pdf::extract(bytes),
        Kind::Docx => office::extract_docx(bytes),
        Kind::Pptx => office::extract_pptx(bytes),
        Kind::Spreadsheet => office::extract_spreadsheet(bytes),
        Kind::Odt | Kind::Odp => office::extract_opendocument(bytes),
        Kind::Epub => epub::extract(bytes),
        Kind::Rtf => rtf::extract(bytes),
        Kind::Notebook => notebook::extract(bytes, opts.notebook_outputs),
        Kind::Npy => npz::extract_npy(bytes),
        Kind::Npz => npz::extract_npz(bytes),
        Kind::Binary => Ok(format!("[binary file, {} bytes]", bytes.len())),
        Kind::Zip | Kind::Tar | Kind::TarGz => Ok(format!(
            "[archive {relative}, {} bytes; pass --archives to expand nested archives]",
            bytes.len()
        )),
        Kind::Text | Kind::Unknown => text::extract(bytes),
    }
}

/// Flatten a zip/tar into `(relative name, bytes)` pairs. Zip-slip (`..`)
/// entries and zip-bomb sized members are skipped.
pub fn expand_archive(
    bytes: &[u8],
    kind: Kind,
    opts: &ExtractOpts,
) -> Result<Vec<(String, Vec<u8>)>, Error> {
    match kind {
        Kind::Zip => archive::expand_zip(bytes, opts),
        Kind::Tar => archive::expand_tar(bytes, opts, false),
        Kind::TarGz => archive::expand_tar(bytes, opts, true),
        _ => Ok(Vec::new()),
    }
}

pub use text::{decode_bytes, extract as extract_text};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classify::Kind;

    #[test]
    fn test_extract_html_in_source_mode_preserves_markup() {
        let opts = ExtractOpts {
            max_file_size: 8 * 1024 * 1024,
            notebook_outputs: false,
            source_mode: true,
        };
        let got = extract("page.html", b"<p>Hi</p>", Kind::Html, &opts).unwrap();
        assert!(got.contains("<p>Hi</p>"), "{got}");
    }

    #[test]
    fn test_extract_html_without_source_mode_strips_tags() {
        let opts = ExtractOpts {
            max_file_size: 8 * 1024 * 1024,
            notebook_outputs: false,
            source_mode: false,
        };
        let got = extract("page.html", b"<p>Hi</p>", Kind::Html, &opts).unwrap();
        assert!(got.contains("Hi"), "{got}");
        assert!(!got.contains("<p>"), "{got}");
    }
}
