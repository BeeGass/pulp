use std::collections::HashMap;
use std::io::{Cursor, Read};

use quick_xml::Reader as XmlReader;
use quick_xml::escape::unescape;
use quick_xml::events::{BytesStart, Event};
use zip::ZipArchive;

const MAX_MEMBER_BYTES: u64 = 32 * 1024 * 1024;
const MAX_CHAPTERS: usize = 500;

/// Extract EPUB chapter text in spine order, falling back to sorted HTML.
pub fn extract(bytes: &[u8]) -> Result<String, crate::error::Error> {
    let mut zip = open_zip(bytes)?;
    let mut chapters = match spine_chapters(&mut zip) {
        Ok(ch) if !ch.is_empty() => ch,
        _ => fallback_chapters(&mut zip)?,
    };
    chapters.truncate(MAX_CHAPTERS);
    render_chapters(&chapters)
}

fn open_zip(bytes: &[u8]) -> Result<ZipArchive<Cursor<&[u8]>>, crate::error::Error> {
    ZipArchive::new(Cursor::new(bytes)).map_err(|err| crate::error::Error::msg(err.to_string()))
}

fn zip_name_slash(name: &str) -> String {
    name.replace('\\', "/")
}

fn lookup_name(zip: &ZipArchive<Cursor<&[u8]>>, path: &str) -> Option<String> {
    let wanted = zip_name_slash(path);
    zip.file_names()
        .find(|name| zip_name_slash(name) == wanted)
        .map(str::to_string)
}

fn read_zip_file(
    zip: &mut ZipArchive<Cursor<&[u8]>>,
    name: &str,
) -> Result<Vec<u8>, crate::error::Error> {
    let file = zip
        .by_name(name)
        .map_err(|err| crate::error::Error::msg(err.to_string()))?;
    let claimed = file.size();
    if claimed > MAX_MEMBER_BYTES {
        return Err(crate::error::Error::msg(format!(
            "epub member {name} is {claimed} bytes; limit {MAX_MEMBER_BYTES}"
        )));
    }
    let mut buf = Vec::new();
    file.take(MAX_MEMBER_BYTES).read_to_end(&mut buf)?;
    Ok(buf)
}

fn read_zip_text(
    zip: &mut ZipArchive<Cursor<&[u8]>>,
    path: &str,
) -> Result<String, crate::error::Error> {
    let name = lookup_name(zip, path)
        .ok_or_else(|| crate::error::Error::msg(format!("zip missing {path}")))?;
    let buf = read_zip_file(zip, &name)?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

fn join_zip_path(base_file: &str, rel: &str) -> String {
    let rel = rel.split(['#', '?']).next().unwrap_or(rel);
    let rel = zip_name_slash(rel);
    if rel.starts_with('/') {
        return normalize_zip_path(&rel);
    }
    let base = zip_name_slash(base_file);
    let dir = base.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
    if dir.is_empty() {
        normalize_zip_path(&rel)
    } else {
        normalize_zip_path(&format!("{dir}/{rel}"))
    }
}

fn normalize_zip_path(path: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                let _ = out.pop();
            }
            p => out.push(p),
        }
    }
    out.join("/")
}

fn attr(e: &BytesStart<'_>, name: &str) -> Option<String> {
    for attr in e.attributes() {
        let Ok(attr) = attr else {
            continue;
        };
        if attr.key.local_name().as_ref() == name {
            return match unescape(attr.value.as_ref()) {
                Ok(s) => Some(s.into_owned()),
                Err(_) => Some(attr.value.into_owned()),
            };
        }
    }
    None
}

fn parse_rootfile(xml: &str) -> Option<String> {
    let mut reader = XmlReader::from_str(xml);
    loop {
        match reader.read_event() {
            Ok(Event::Start(e) | Event::Empty(e)) if e.local_name().as_ref() == "rootfile" => {
                return attr(&e, "full-path");
            }
            Ok(Event::Eof) => return None,
            Err(_) => return None,
            _ => {}
        }
    }
}

struct OpfItem {
    href: String,
    media_type: String,
}

fn parse_opf(xml: &str) -> (HashMap<String, OpfItem>, Vec<String>) {
    let mut reader = XmlReader::from_str(xml);
    let mut manifest = HashMap::new();
    let mut spine = Vec::new();
    loop {
        match reader.read_event() {
            Ok(Event::Start(e) | Event::Empty(e)) => match e.local_name().as_ref() {
                "item" => {
                    if let (Some(id), Some(href)) = (attr(&e, "id"), attr(&e, "href")) {
                        let media_type = attr(&e, "media-type").unwrap_or_default();
                        manifest.insert(id, OpfItem { href, media_type });
                    }
                }
                "itemref" => {
                    if let Some(idref) = attr(&e, "idref") {
                        spine.push(idref);
                    }
                }
                _ => {}
            },
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
    }
    (manifest, spine)
}

fn is_html_item(href: &str, media_type: &str) -> bool {
    let media = media_type.to_ascii_lowercase();
    if media.contains("html") {
        return true;
    }
    let href = href.to_ascii_lowercase();
    href.ends_with(".xhtml") || href.ends_with(".html") || href.ends_with(".htm")
}

fn is_html_path(path: &str) -> bool {
    let lower = zip_name_slash(path).to_ascii_lowercase();
    lower.ends_with(".xhtml") || lower.ends_with(".html")
}

fn spine_chapters(
    zip: &mut ZipArchive<Cursor<&[u8]>>,
) -> Result<Vec<(String, String)>, crate::error::Error> {
    let container = read_zip_text(zip, "META-INF/container.xml")?;
    let opf_path = parse_rootfile(&container)
        .ok_or_else(|| crate::error::Error::msg("epub missing rootfile"))?;
    let opf = read_zip_text(zip, &opf_path)?;
    let (manifest, spine) = parse_opf(&opf);
    let mut chapters = Vec::new();
    for idref in spine {
        let Some(item) = manifest.get(&idref) else {
            continue;
        };
        if !is_html_item(&item.href, &item.media_type) {
            continue;
        }
        let zip_path = join_zip_path(&opf_path, &item.href);
        let Ok(html) = read_zip_text(zip, &zip_path) else {
            continue;
        };
        chapters.push((item.href.clone(), html));
    }
    Ok(chapters)
}

fn fallback_chapters(
    zip: &mut ZipArchive<Cursor<&[u8]>>,
) -> Result<Vec<(String, String)>, crate::error::Error> {
    let mut names: Vec<String> = zip
        .file_names()
        .filter(|name| is_html_path(name))
        .map(str::to_string)
        .collect();
    names.sort();
    let mut chapters = Vec::new();
    for name in names {
        let html = read_zip_text(zip, &name)?;
        chapters.push((name, html));
    }
    Ok(chapters)
}

fn render_chapters(chapters: &[(String, String)]) -> Result<String, crate::error::Error> {
    let mut out = String::new();
    for (href, html) in chapters {
        let text = html2text::from_read(html.as_bytes(), 100)
            .map_err(|err| crate::error::Error::msg(err.to_string()))?;
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str("# ");
        out.push_str(href);
        out.push_str("\n\n");
        out.push_str(text.trim());
    }
    Ok(out)
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
    fn test_extract_with_spine_order_returns_headed_chapters() {
        let container = br#"<?xml version="1.0"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles>
    <rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/>
  </rootfiles>
</container>"#;
        let opf = br#"<?xml version="1.0"?>
<package xmlns="http://www.idpf.org/2007/opf" unique-identifier="id" version="3.0">
  <manifest>
    <item id="c2" href="c2.xhtml" media-type="application/xhtml+xml"/>
    <item id="c1" href="c1.xhtml" media-type="application/xhtml+xml"/>
  </manifest>
  <spine>
    <itemref idref="c1"/>
    <itemref idref="c2"/>
  </spine>
</package>"#;
        let c1 = b"<html><body><p>First chapter</p></body></html>";
        let c2 = b"<html><body><p>Second chapter</p></body></html>";
        let bytes = zip_bytes(&[
            ("META-INF/container.xml", container),
            ("OEBPS/content.opf", opf),
            ("OEBPS/c1.xhtml", c1),
            ("OEBPS/c2.xhtml", c2),
        ]);
        let text = extract(&bytes).expect("epub extract");
        assert!(text.contains("# c1.xhtml"), "{text}");
        assert!(text.contains("# c2.xhtml"), "{text}");
        let i1 = text.find("# c1.xhtml").expect("c1 heading");
        let i2 = text.find("# c2.xhtml").expect("c2 heading");
        assert!(i1 < i2, "spine order, got {text}");
        assert!(text.contains("First chapter"), "{text}");
        assert!(text.contains("Second chapter"), "{text}");
    }

    #[test]
    fn test_extract_with_missing_opf_returns_sorted_html_fallback() {
        let b = b"<html><body><p>Bee</p></body></html>";
        let a = b"<html><body><p>Aye</p></body></html>";
        let bytes = zip_bytes(&[("z.xhtml", b), ("a.xhtml", a)]);
        let text = extract(&bytes).expect("epub fallback");
        let ia = text.find("# a.xhtml").expect("a heading");
        let iz = text.find("# z.xhtml").expect("z heading");
        assert!(ia < iz, "sorted fallback, got {text}");
        assert!(text.contains("Aye"), "{text}");
        assert!(text.contains("Bee"), "{text}");
    }
}
