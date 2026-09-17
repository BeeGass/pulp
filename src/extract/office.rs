use std::io::{Cursor, Read};

use calamine::Reader;
use quick_xml::Reader as XmlReader;
use quick_xml::escape::{resolve_predefined_entity, unescape};
use quick_xml::events::{BytesRef, Event};
use zip::ZipArchive;

const MARKDOWN_TABLE_MAX_COLS: usize = 24;
const MAX_MEMBER_BYTES: u64 = 32 * 1024 * 1024;
const MAX_PPTX_SLIDES: usize = 200;

/// Extract text from a `.docx` (`word/document.xml`).
pub fn extract_docx(bytes: &[u8]) -> Result<String, crate::error::Error> {
    let mut zip = open_zip(bytes)?;
    let xml = read_zip_xml(&mut zip, "word/document.xml")?;
    Ok(docx_from_xml(&xml)?.trim().to_string())
}

/// Extract text from a `.pptx`, one `## Slide N` section per slide XML.
pub fn extract_pptx(bytes: &[u8]) -> Result<String, crate::error::Error> {
    let mut zip = open_zip(bytes)?;
    let mut slides: Vec<(u32, String)> = zip
        .file_names()
        .filter_map(|name| slide_num(name).map(|n| (n, name.to_string())))
        .collect();
    slides.sort_by_key(|(n, _)| *n);

    slides.truncate(MAX_PPTX_SLIDES);
    let mut out = String::new();
    let mut total = 0u64;
    for (n, name) in slides {
        let xml = read_zip_xml_by_name(&mut zip, &name)?;
        total = total.saturating_add(xml.len() as u64);
        if total > MAX_MEMBER_BYTES.saturating_mul(4) {
            break;
        }
        let text = pptx_from_xml(&xml)?;
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str("## Slide ");
        out.push_str(&n.to_string());
        out.push('\n');
        let trimmed = text.trim();
        if !trimmed.is_empty() {
            out.push_str(trimmed);
            out.push('\n');
        }
    }
    Ok(out.trim().to_string())
}

/// Extract sheets from xls/xlsx/xlsb/ods via calamine.
pub fn extract_spreadsheet(bytes: &[u8]) -> Result<String, crate::error::Error> {
    let cursor = Cursor::new(bytes.to_vec());
    let mut workbook = match calamine::open_workbook_auto_from_rs(cursor) {
        Ok(wb) => wb,
        Err(err) => {
            if let Ok(text) = extract_opendocument(bytes) {
                if !text.trim().is_empty() {
                    return Ok(text);
                }
            }
            return Err(crate::error::Error::msg(err.to_string()));
        }
    };
    let names = workbook.sheet_names();
    let mut out = String::new();
    for (i, name) in names.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str("## ");
        out.push_str(name);
        out.push('\n');
        let range = workbook
            .worksheet_range(name)
            .map_err(|err| crate::error::Error::msg(err.to_string()))?;
        write_sheet(&mut out, &range);
    }
    Ok(out.trim().to_string())
}

/// Extract text from ODT/ODP `content.xml`.
pub fn extract_opendocument(bytes: &[u8]) -> Result<String, crate::error::Error> {
    let mut zip = open_zip(bytes)?;
    let xml = read_zip_xml(&mut zip, "content.xml")?;
    Ok(opendocument_from_xml(&xml)?.trim().to_string())
}

fn open_zip(bytes: &[u8]) -> Result<ZipArchive<Cursor<&[u8]>>, crate::error::Error> {
    ZipArchive::new(Cursor::new(bytes)).map_err(|err| crate::error::Error::msg(err.to_string()))
}

fn zip_name_slash(name: &str) -> String {
    name.replace('\\', "/")
}

fn find_zip_name(zip: &ZipArchive<Cursor<&[u8]>>, wanted: &str) -> Option<String> {
    zip.file_names()
        .find(|name| zip_name_slash(name) == wanted)
        .map(str::to_string)
}

fn read_zip_xml(
    zip: &mut ZipArchive<Cursor<&[u8]>>,
    wanted: &str,
) -> Result<String, crate::error::Error> {
    let name = find_zip_name(zip, wanted)
        .ok_or_else(|| crate::error::Error::msg(format!("zip missing {wanted}")))?;
    read_zip_xml_by_name(zip, &name)
}

fn read_zip_xml_by_name(
    zip: &mut ZipArchive<Cursor<&[u8]>>,
    name: &str,
) -> Result<String, crate::error::Error> {
    let file = zip
        .by_name(name)
        .map_err(|err| crate::error::Error::msg(err.to_string()))?;
    let claimed = file.size();
    if claimed > MAX_MEMBER_BYTES {
        return Err(crate::error::Error::msg(format!(
            "office member {name} is {claimed} bytes; limit {MAX_MEMBER_BYTES}"
        )));
    }
    let mut buf = Vec::new();
    file.take(MAX_MEMBER_BYTES).read_to_end(&mut buf)?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

fn slide_num(path: &str) -> Option<u32> {
    let path = zip_name_slash(path);
    let name = path.strip_prefix("ppt/slides/")?;
    let name = name.strip_suffix(".xml")?;
    let digits = name.strip_prefix("slide")?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

fn push_xml_text(out: &mut String, raw: &str) {
    match unescape(raw) {
        Ok(s) => out.push_str(&s),
        Err(_) => out.push_str(raw),
    }
}

fn push_xml_ref(out: &mut String, r: &BytesRef<'_>) {
    if let Ok(Some(ch)) = r.resolve_char_ref() {
        out.push(ch);
        return;
    }
    if let Some(ent) = resolve_predefined_entity(r.as_ref()) {
        out.push_str(ent);
    }
}

fn docx_from_xml(xml: &str) -> Result<String, crate::error::Error> {
    let mut reader = XmlReader::from_str(xml);
    let mut out = String::new();
    let mut in_t = false;
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => match e.local_name().as_ref() {
                "t" => in_t = true,
                "tab" => out.push('\t'),
                "br" => out.push('\n'),
                _ => {}
            },
            Ok(Event::Empty(e)) => match e.local_name().as_ref() {
                "tab" => out.push('\t'),
                "br" | "p" => out.push('\n'),
                _ => {}
            },
            Ok(Event::End(e)) => match e.local_name().as_ref() {
                "t" => in_t = false,
                "p" => out.push('\n'),
                _ => {}
            },
            Ok(Event::Text(t)) if in_t => push_xml_text(&mut out, t.as_ref()),
            Ok(Event::GeneralRef(r)) if in_t => push_xml_ref(&mut out, &r),
            Ok(Event::CData(t)) if in_t => out.push_str(t.as_ref()),
            Ok(Event::Eof) => break,
            Err(err) => return Err(crate::error::Error::msg(err.to_string())),
            _ => {}
        }
    }
    Ok(out)
}

fn pptx_from_xml(xml: &str) -> Result<String, crate::error::Error> {
    let mut reader = XmlReader::from_str(xml);
    let mut out = String::new();
    let mut in_t = false;
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => match e.local_name().as_ref() {
                "t" => in_t = true,
                "br" => out.push('\n'),
                _ => {}
            },
            Ok(Event::Empty(e)) if e.local_name().as_ref() == "br" => out.push('\n'),
            Ok(Event::End(e)) => match e.local_name().as_ref() {
                "t" => in_t = false,
                "p" => out.push('\n'),
                _ => {}
            },
            Ok(Event::Text(t)) if in_t => push_xml_text(&mut out, t.as_ref()),
            Ok(Event::GeneralRef(r)) if in_t => push_xml_ref(&mut out, &r),
            Ok(Event::CData(t)) if in_t => out.push_str(t.as_ref()),
            Ok(Event::Eof) => break,
            Err(err) => return Err(crate::error::Error::msg(err.to_string())),
            _ => {}
        }
    }
    Ok(out)
}

fn opendocument_from_xml(xml: &str) -> Result<String, crate::error::Error> {
    let mut reader = XmlReader::from_str(xml);
    let mut out = String::new();
    let mut para_depth: u32 = 0;
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => match e.local_name().as_ref() {
                "p" | "h" => para_depth = para_depth.saturating_add(1),
                "line-break" => out.push('\n'),
                _ => {}
            },
            Ok(Event::Empty(e)) if e.local_name().as_ref() == "line-break" => out.push('\n'),
            Ok(Event::End(e)) => {
                if matches!(e.local_name().as_ref(), "p" | "h") {
                    para_depth = para_depth.saturating_sub(1);
                    out.push('\n');
                }
            }
            Ok(Event::Text(t)) if para_depth > 0 => push_xml_text(&mut out, t.as_ref()),
            Ok(Event::GeneralRef(r)) if para_depth > 0 => push_xml_ref(&mut out, &r),
            Ok(Event::CData(t)) if para_depth > 0 => out.push_str(t.as_ref()),
            Ok(Event::Eof) => break,
            Err(err) => return Err(crate::error::Error::msg(err.to_string())),
            _ => {}
        }
    }
    Ok(out)
}

fn write_sheet(out: &mut String, range: &calamine::Range<calamine::Data>) {
    let width = range.width();
    if width == 0 {
        return;
    }
    let markdown = width <= MARKDOWN_TABLE_MAX_COLS;
    for (i, row) in range.rows().enumerate() {
        let cells: Vec<String> = row.iter().map(ToString::to_string).collect();
        if markdown {
            out.push_str("| ");
            out.push_str(&cells.join(" | "));
            out.push_str(" |\n");
            if i == 0 {
                out.push_str("| ");
                out.push_str(&vec!["---"; width].join(" | "));
                out.push_str(" |\n");
            }
        } else {
            out.push_str(&cells.join(" | "));
            out.push('\n');
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Write};

    fn zip_bytes(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        for (name, data) in files {
            writer.start_file(*name, options).unwrap();
            writer.write_all(data).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    #[test]
    fn test_extract_docx_with_paragraphs_and_tab_returns_text() {
        let xml = br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
  <w:body>
    <w:p>
      <w:r>
        <w:t>Hello</w:t>
        <w:tab/>
        <w:t>World &amp; Co</w:t>
      </w:r>
    </w:p>
    <w:p>
      <w:r>
        <w:t>Second</w:t>
        <w:br/>
        <w:t>line</w:t>
      </w:r>
    </w:p>
  </w:body>
</w:document>"#;
        let bytes = zip_bytes(&[("word/document.xml", xml)]);
        let text = extract_docx(&bytes).expect("docx extract");
        assert_eq!(text, "Hello\tWorld & Co\nSecond\nline");
    }

    #[test]
    fn test_extract_pptx_with_slides_returns_numbered_headings() {
        let slide2 = br#"<?xml version="1.0" encoding="UTF-8"?>
<p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"
       xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">
  <p:cSld><p:spTree>
    <p:sp><p:txBody><a:p><a:r><a:t>Two</a:t></a:r></a:p></p:txBody></p:sp>
  </p:spTree></p:cSld>
</p:sld>"#;
        let slide1 = br#"<?xml version="1.0" encoding="UTF-8"?>
<p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"
       xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">
  <p:cSld><p:spTree>
    <p:sp><p:txBody><a:p><a:r><a:t>One</a:t></a:r></a:p></p:txBody></p:sp>
  </p:spTree></p:cSld>
</p:sld>"#;
        let bytes = zip_bytes(&[
            ("ppt/slides/slide2.xml", slide2),
            ("ppt/slides/slide1.xml", slide1),
        ]);
        let text = extract_pptx(&bytes).expect("pptx extract");
        assert_eq!(text, "## Slide 1\nOne\n\n## Slide 2\nTwo");
    }

    #[test]
    fn test_extract_opendocument_with_headings_returns_text() {
        let xml = br#"<?xml version="1.0" encoding="UTF-8"?>
<office:document-content
  xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
  xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0">
  <office:body>
    <text:h>Title</text:h>
    <text:p>Hello<text:line-break/>World</text:p>
  </office:body>
</office:document-content>"#;
        let bytes = zip_bytes(&[("content.xml", xml)]);
        let text = extract_opendocument(&bytes).expect("odt extract");
        assert_eq!(text, "Title\nHello\nWorld");
    }

    #[test]
    fn test_extract_spreadsheet_with_ods_returns_markdown_table() {
        let mimetype = b"application/vnd.oasis.opendocument.spreadsheet";
        let manifest = br#"<?xml version="1.0" encoding="UTF-8"?>
<manifest:manifest xmlns:manifest="urn:oasis:names:tc:opendocument:xmlns:manifest:1.0"/>"#;
        let content = br#"<?xml version="1.0" encoding="UTF-8"?>
<office:document-content
  xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
  xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"
  xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0">
  <office:body>
    <office:spreadsheet>
      <table:table table:name="Sheet1">
        <table:table-row>
          <table:table-cell office:value-type="string"><text:p>Name</text:p></table:table-cell>
          <table:table-cell office:value-type="string"><text:p>Age</text:p></table:table-cell>
        </table:table-row>
        <table:table-row>
          <table:table-cell office:value-type="string"><text:p>Ada</text:p></table:table-cell>
          <table:table-cell office:value-type="string"><text:p>36</text:p></table:table-cell>
        </table:table-row>
      </table:table>
    </office:spreadsheet>
  </office:body>
</office:document-content>"#;
        let bytes = zip_bytes(&[
            ("mimetype", mimetype),
            ("META-INF/manifest.xml", manifest),
            ("content.xml", content),
        ]);
        let text = extract_spreadsheet(&bytes).expect("ods extract");
        assert!(text.contains("Name"), "{text}");
        assert!(text.contains("Ada"), "{text}");
        assert!(text.contains("36"), "{text}");
    }
}
