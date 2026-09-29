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
/// A parser failure comes back as [`Error::Unreadable`] with the parser's
/// message. Packing keeps a short note in the file's place, so one odd file
/// does not fail the pack, and flags the file.
pub fn extract(
    relative: &str,
    bytes: &[u8],
    kind: Kind,
    opts: &ExtractOpts,
) -> Result<String, Error> {
    extract_kind(relative, bytes, kind, opts).map_err(|err| Error::Unreadable(err.to_string()))
}

fn extract_kind(
    relative: &str,
    bytes: &[u8],
    kind: Kind,
    opts: &ExtractOpts,
) -> Result<String, Error> {
    if let Err(refusal) = check_signature(bytes, kind) {
        return read_misnamed(relative, bytes, opts, refusal);
    }
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

/// A document whose bytes are not what its name says, `refusal` saying what
/// they are instead.
///
/// A failed download usually leaves a web page, or a line of text, under the
/// document's name. Its text is still worth a dump, so it is read as what it
/// holds, under a note naming the mismatch: `[not a PDF: it holds an HTML page
/// ("Preparing to download ...")]`. An empty file is the note alone. Anything
/// else (an image, some other format) stays unreadable with the refusal.
fn read_misnamed(
    relative: &str,
    bytes: &[u8],
    opts: &ExtractOpts,
    refusal: Error,
) -> Result<String, Error> {
    let actual = if bytes.is_empty() {
        None
    } else if is_html_page(bytes) {
        Some(Kind::Html)
    } else if infer::get(bytes).is_none() && !crate::classify::looks_binary(bytes) {
        Some(Kind::Text)
    } else {
        return Err(refusal);
    };
    let note = match refusal {
        Error::Unreadable(reason) => format!("[{reason}]"),
        other => format!("[{other}]"),
    };
    match actual {
        Some(kind) => {
            let text = extract_kind(relative, bytes, kind, opts)?;
            Ok(format!("{note}\n{text}"))
        }
        None => Ok(note),
    }
}

/// How far into a PDF its `%PDF-` header may start. Readers accept other
/// bytes before it, a mail header say, within the first 1024.
const PDF_HEADER_WITHIN: usize = 1024;

/// Most characters of a page title that a note quotes, "..." included.
const TITLE_CHARS: usize = 60;

/// Bytes of a page searched for its `<title>`.
const TITLE_SEARCH_BYTES: usize = 64 * 1024;

/// Most bytes of a title that are decoded.
const TITLE_RAW_BYTES: usize = 1024;

/// Refuse bytes that cannot be a `kind` document before its parser sees
/// them, with an [`Error::Unreadable`] that says what they hold instead.
///
/// A failed download often leaves a web page under the name of the document
/// it promised, and a parser's complaint about that ("invalid file header")
/// does not say so. The note does: `not a PDF: it holds an HTML page
/// ("Preparing to download ...")`. Kinds without a fixed signature pass.
pub(crate) fn check_signature(bytes: &[u8], kind: Kind) -> Result<(), Error> {
    const ZIP: &[u8] = b"PK\x03\x04";
    // Excel 97-2003 workbooks are OLE compound files, not zips.
    const OLE: &[u8] = b"\xd0\xcf\x11\xe0\xa1\xb1\x1a\xe1";
    let (noun, signed) = match kind {
        Kind::Pdf => ("a PDF", has_pdf_header(bytes)),
        Kind::Docx => ("a Word document", bytes.starts_with(ZIP)),
        Kind::Pptx => ("a PowerPoint presentation", bytes.starts_with(ZIP)),
        Kind::Spreadsheet => (
            "a spreadsheet",
            bytes.starts_with(ZIP) || bytes.starts_with(OLE),
        ),
        Kind::Odt => ("an OpenDocument text", bytes.starts_with(ZIP)),
        Kind::Odp => ("an OpenDocument presentation", bytes.starts_with(ZIP)),
        Kind::Epub => ("an EPUB", bytes.starts_with(ZIP)),
        _ => return Ok(()),
    };
    if signed {
        Ok(())
    } else {
        Err(Error::Unreadable(format!(
            "not {noun}: {}",
            contents(bytes)
        )))
    }
}

/// Whether an unreadable `reason` is [`check_signature`]'s refusal of a file
/// whose bytes are not what its name says (`not a PDF: it holds ...`).
pub(crate) fn is_misnamed(reason: &str) -> bool {
    reason.starts_with("not a ") || reason.starts_with("not an ")
}

/// Whether `%PDF-` starts within the first [`PDF_HEADER_WITHIN`] bytes.
fn has_pdf_header(bytes: &[u8]) -> bool {
    const HEADER: &[u8] = b"%PDF-";
    let window = &bytes[..bytes.len().min(PDF_HEADER_WITHIN + HEADER.len() - 1)];
    window.windows(HEADER.len()).any(|at| at == HEADER)
}

/// What `bytes` hold, as a note says it: "the file is empty", "it holds an
/// HTML page (\"Title\")", "it holds a PNG image", "it holds plain text".
fn contents(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return "the file is empty".to_string();
    }
    if is_html_page(bytes) {
        return match page_title(bytes) {
            Some(title) => format!("it holds an HTML page (\"{title}\")"),
            None => "it holds an HTML page".to_string(),
        };
    }
    let noun = match infer::get(bytes) {
        Some(found) => type_noun(&found),
        None if crate::classify::looks_binary(bytes) => {
            "binary data of an unknown type".to_string()
        }
        None => "plain text".to_string(),
    };
    format!("it holds {noun}")
}

/// Whether `bytes` open as an HTML page: with an HTML tag, doctype, or
/// comment, as the MIME sniffing standard reads them, or as XHTML, an XML
/// prolog that an `<html>` element follows.
fn is_html_page(bytes: &[u8]) -> bool {
    let bytes = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(bytes);
    infer::text::is_html(bytes)
        || (infer::text::is_xml(bytes)
            && find_ignore_case(&bytes[..bytes.len().min(1024)], b"<html").is_some())
}

/// A page's first `<title>` as one line of at most [`TITLE_CHARS`]
/// characters, its entities decoded and its whitespace folded. `None` when
/// the page has none or it is blank.
fn page_title(page: &[u8]) -> Option<String> {
    let head = &page[..page.len().min(TITLE_SEARCH_BYTES)];
    let tag = &head[find_ignore_case(head, b"<title")? + b"<title".len()..];
    // `<title>` or `<title lang="en">`, never `<titles>`.
    if !tag
        .first()
        .is_some_and(|b| *b == b'>' || b.is_ascii_whitespace())
    {
        return None;
    }
    let body = &tag[tag.iter().position(|b| *b == b'>')? + 1..];
    let mut raw = &body[..find_ignore_case(body, b"</title").unwrap_or(body.len())];
    if raw.len() > TITLE_RAW_BYTES {
        raw = &raw[..TITLE_RAW_BYTES];
        // A UTF-8 character cut in two would send the whole title to the
        // legacy encoding guess.
        if let Err(err) = std::str::from_utf8(raw)
            && err.error_len().is_none()
        {
            raw = &raw[..err.valid_up_to()];
        }
    }
    // The HTML extractor decodes the title as it decodes a page.
    let text = html::extract(raw).ok()?;
    let line: String = text
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .filter(|c| !c.is_control())
        .collect();
    (!line.is_empty()).then(|| shorten(&line, TITLE_CHARS))
}

/// `text` cut to at most `max` characters on a word break where there is
/// one, "..." marking the cut.
fn shorten(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut chars = text.chars();
    let cut: String = chars.by_ref().take(max - 3).collect();
    let kept = if chars.next().is_none_or(char::is_whitespace) {
        cut.as_str()
    } else {
        cut.rfind(' ').map_or(cut.as_str(), |at| &cut[..at])
    };
    format!("{}...", kept.trim_end())
}

/// Where `needle` first occurs in `haystack`, ASCII case ignored.
fn find_ignore_case(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|at| at.eq_ignore_ascii_case(needle))
}

/// Nouns for the types `infer` recognizes, by the extension it names.
const TYPE_NOUNS: &[(&str, &str)] = &[
    ("pdf", "a PDF"),
    ("docx", "a Word document"),
    ("doc", "a Word 97-2003 document"),
    ("xlsx", "an Excel spreadsheet"),
    ("xls", "an Excel 97-2003 spreadsheet"),
    ("pptx", "a PowerPoint presentation"),
    ("ppt", "a PowerPoint 97-2003 presentation"),
    ("odt", "an OpenDocument text"),
    ("ods", "an OpenDocument spreadsheet"),
    ("odp", "an OpenDocument presentation"),
    ("epub", "an EPUB"),
    ("rtf", "an RTF document"),
    ("zip", "a zip archive"),
    ("tar", "a tar archive"),
    ("gz", "a gzip file"),
    ("bz2", "a bzip2 file"),
    ("xz", "an xz file"),
    ("zst", "a zstd file"),
    ("7z", "a 7-Zip archive"),
    ("rar", "a RAR archive"),
    ("png", "a PNG image"),
    ("jpg", "a JPEG image"),
    ("gif", "a GIF image"),
    ("webp", "a WebP image"),
    ("tif", "a TIFF image"),
    ("bmp", "a BMP image"),
    ("heif", "a HEIF image"),
    ("avif", "an AVIF image"),
    ("ico", "an icon"),
    ("mp4", "an MP4 video"),
    ("mov", "a QuickTime video"),
    ("webm", "a WebM video"),
    ("mp3", "an MP3 audio file"),
    ("wav", "a WAV audio file"),
    ("exe", "a Windows program"),
    ("dll", "a Windows library"),
    ("elf", "an ELF program"),
    ("mach", "a Mach-O program"),
    ("wasm", "a WebAssembly module"),
    ("sqlite", "an SQLite database"),
    ("xml", "an XML document"),
    ("sh", "a shell script"),
];

/// A noun for a type `infer` recognized: "a PNG image", or for a type
/// without one of its own, its family and MIME type.
fn type_noun(found: &infer::Type) -> String {
    if let Some((_, noun)) = TYPE_NOUNS
        .iter()
        .find(|(extension, _)| *extension == found.extension())
    {
        return (*noun).to_string();
    }
    let family = match found.matcher_type() {
        infer::MatcherType::App => "a program",
        infer::MatcherType::Archive => "an archive",
        infer::MatcherType::Audio => "an audio file",
        infer::MatcherType::Book => "an e-book",
        infer::MatcherType::Doc => "a document",
        infer::MatcherType::Font => "a font",
        infer::MatcherType::Image => "an image",
        infer::MatcherType::Text => "text",
        infer::MatcherType::Video => "a video",
        infer::MatcherType::Custom => "data",
    };
    format!("{family} ({})", found.mime_type())
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

/// Expand a zip/tar into members, drawing on a budget shared by every
/// archive nested under one top-level input. `want` judges each member by
/// name before it is read. Oversized members come back named and sized,
/// without their bytes, and members that cannot be read come back as notes.
pub(crate) fn expand_archive_members(
    bytes: &[u8],
    kind: Kind,
    opts: &ExtractOpts,
    budget: &mut ArchiveBudget,
    want: &dyn Fn(&str) -> Want,
) -> Result<Vec<Member>, Error> {
    match kind {
        Kind::Zip => archive::expand_zip_members(bytes, opts, budget, want),
        Kind::Tar => archive::expand_tar_members(bytes, opts, false, budget, want),
        Kind::TarGz => archive::expand_tar_members(bytes, opts, true, budget, want),
        _ => Ok(Vec::new()),
    }
}

pub(crate) use archive::{ArchiveBudget, Member, Want};
pub(crate) use text::sniff_wide_text;
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

    fn default_opts() -> ExtractOpts {
        ExtractOpts {
            max_file_size: 8 * 1024 * 1024,
            notebook_outputs: false,
            source_mode: false,
        }
    }

    /// The reason `extract` gives for refusing `bytes` as `kind`.
    fn refusal(bytes: &[u8], kind: Kind) -> String {
        match extract("file", bytes, kind, &default_opts()) {
            Err(Error::Unreadable(reason)) => reason,
            other => panic!("expected an unreadable error, got {other:?}"),
        }
    }

    /// What `extract` reads from bytes that are not a `kind` document, split
    /// into its note line and the text after it.
    fn misnamed(bytes: &[u8], kind: Kind) -> (String, String) {
        let text = extract("file", bytes, kind, &default_opts())
            .unwrap_or_else(|err| panic!("expected text under a note, got {err:?}"));
        let (note, rest) = text.split_once('\n').unwrap_or((&text, ""));
        (note.to_string(), rest.to_string())
    }

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\0\x01\0\0\0\x01\x08\x02\0\0\0";

    #[test]
    fn test_extract_with_html_page_saved_as_pdf_returns_its_text_under_a_note() {
        let page = b"<!DOCTYPE html>\n<html><head><meta charset=\"utf-8\">\n\
            <title>Preparing to download ...</title></head>\n\
            <body><p>Your file will start shortly.</p></body></html>\n";
        let (note, text) = misnamed(page, Kind::Pdf);
        assert_eq!(
            note,
            "[not a PDF: it holds an HTML page (\"Preparing to download ...\")]"
        );
        assert!(text.contains("Your file will start shortly."), "{text}");
        let untitled = b"  <html><body><p>Sign in to continue</p></body></html>";
        let (note, text) = misnamed(untitled, Kind::Pdf);
        assert_eq!(note, "[not a PDF: it holds an HTML page]");
        assert!(text.contains("Sign in to continue"), "{text}");
    }

    #[test]
    fn test_extract_with_html_page_saved_as_pdf_in_source_mode_returns_its_source() {
        let page = b"<html><head><title>Notice</title></head><body><p>Moved</p></body></html>";
        let opts = ExtractOpts {
            source_mode: true,
            ..default_opts()
        };
        let text = extract("file", page, Kind::Pdf, &opts).unwrap();
        assert_eq!(
            text,
            format!(
                "[not a PDF: it holds an HTML page (\"Notice\")]\n{}",
                String::from_utf8_lossy(page)
            )
        );
    }

    #[test]
    fn test_extract_with_text_saved_as_docx_returns_the_text_under_a_note() {
        let (note, text) = misnamed(b"Meeting notes\n- tide tables\n- storm log\n", Kind::Docx);
        assert_eq!(note, "[not a Word document: it holds plain text]");
        assert_eq!(text.trim_end(), "Meeting notes\n- tide tables\n- storm log");
    }

    #[test]
    fn test_extract_with_empty_epub_returns_the_note_alone() {
        let text = extract("file", b"", Kind::Epub, &default_opts()).unwrap();
        assert_eq!(text, "[not an EPUB: the file is empty]");
    }

    #[test]
    fn test_is_misnamed_with_signature_refusal_and_parser_error_tells_them_apart() {
        assert!(is_misnamed("not a PDF: it holds a PNG image"));
        assert!(is_misnamed("not an EPUB: the file is empty"));
        assert!(!is_misnamed(
            "PDF error: couldn't parse input: invalid xref"
        ));
    }

    #[test]
    fn test_extract_with_other_types_under_document_names_returns_note_naming_the_type() {
        assert_eq!(refusal(PNG, Kind::Pdf), "not a PDF: it holds a PNG image");
        assert_eq!(
            refusal(b"%PDF-1.7\n%\xe2\xe3\xcf\xd3\n1 0 obj\n", Kind::Pptx),
            "not a PowerPoint presentation: it holds a PDF"
        );
        let xhtml = b"<?xml version=\"1.0\"?>\n<html xmlns=\"http://www.w3.org/1999/xhtml\">\
            <head><title>Access denied</title></head></html>";
        assert_eq!(
            misnamed(xhtml, Kind::Odt).0,
            "[not an OpenDocument text: it holds an HTML page (\"Access denied\")]"
        );
        let noise: Vec<u8> = (0..=255u8).cycle().skip(7).take(2048).collect();
        assert_eq!(
            refusal(&noise, Kind::Spreadsheet),
            "not a spreadsheet: it holds binary data of an unknown type"
        );
    }

    #[test]
    fn test_extract_with_long_title_holding_entities_and_breaks_returns_one_short_line() {
        let page = format!(
            "<html><head><title lang=\"en\">\n  Q3 &amp; Q4\n\ttide report {} </TITLE>",
            "and more ".repeat(20)
        );
        let (note, _) = misnamed(page.as_bytes(), Kind::Epub);
        let title = note
            .strip_prefix("[not an EPUB: it holds an HTML page (\"")
            .and_then(|rest| rest.strip_suffix("\")]"))
            .unwrap_or_else(|| panic!("{note}"));
        // Cut on the last word break within 57 characters, then "...".
        assert_eq!(
            title,
            "Q3 & Q4 tide report and more and more and more and more..."
        );
        assert!(title.chars().count() <= TITLE_CHARS, "{title}");
    }

    #[test]
    fn test_extract_with_signature_behind_junk_or_ole2_header_reaches_the_parser() {
        // A PDF header may sit anywhere in the first 1024 bytes.
        let late = b"\r\n\r\n%PDF-1.4 garbage";
        assert_eq!(
            refusal(late, Kind::Pdf),
            pdf::extract(late).unwrap_err().to_string()
        );
        let mut too_late = vec![b' '; 1024];
        too_late.extend_from_slice(b"%PDF-1.4 garbage");
        assert_eq!(
            misnamed(&too_late, Kind::Pdf).0,
            "[not a PDF: it holds plain text]"
        );
        // An old binary spreadsheet is a spreadsheet, not a zip.
        let mut xls = b"\xd0\xcf\x11\xe0\xa1\xb1\x1a\xe1".to_vec();
        xls.extend_from_slice(&[0u8; 504]);
        let reason = refusal(&xls, Kind::Spreadsheet);
        assert!(!reason.starts_with("not a spreadsheet"), "{reason}");
    }

    #[test]
    fn test_extract_with_garbage_pdf_returns_unreadable_error() {
        let opts = ExtractOpts {
            max_file_size: 8 * 1024 * 1024,
            notebook_outputs: false,
            source_mode: false,
        };
        let err = extract("paper.pdf", b"%PDF-1.4 garbage", Kind::Pdf, &opts).unwrap_err();
        match err {
            Error::Unreadable(reason) => assert!(!reason.trim().is_empty()),
            other => panic!("expected an unreadable error, got {other:?}"),
        }
    }
}
