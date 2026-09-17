//! HTML to plain text.

use quick_xml::events::Event;

use crate::error::Error;

/// Convert HTML bytes to readable text.
///
/// Tries html2text first (`from_read`, width 100). On failure, falls back to
/// concatenating quick-xml text events.
pub fn extract(bytes: &[u8]) -> Result<String, Error> {
    match html2text::from_read(bytes, 100) {
        Ok(text) => Ok(text),
        Err(_) => Ok(strip_tags(bytes)),
    }
}

fn strip_tags(bytes: &[u8]) -> String {
    let decoded = super::text::decode_bytes(bytes);
    let mut reader = quick_xml::Reader::from_str(&decoded);
    reader.config_mut().allow_dangling_amp = true;
    reader.config_mut().allow_unmatched_ends = true;
    reader.config_mut().check_end_names = false;

    let mut out = String::new();
    loop {
        match reader.read_event() {
            Ok(Event::Text(e)) => push_text(&mut out, e.as_ref()),
            Ok(Event::CData(e)) => out.push_str(e.as_ref()),
            Ok(Event::GeneralRef(r)) => push_entity(&mut out, &r),
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(_) => return decoded,
        }
    }
    out
}

fn push_text(out: &mut String, raw: &str) {
    match quick_xml::escape::unescape(raw) {
        Ok(text) => out.push_str(&text),
        Err(_) => out.push_str(raw),
    }
}

fn push_entity(out: &mut String, r: &quick_xml::events::BytesRef<'_>) {
    match r.resolve_char_ref() {
        Ok(Some(ch)) => out.push(ch),
        Ok(None) => {
            if let Some(ent) = quick_xml::escape::resolve_xml_entity(r.as_ref()) {
                out.push_str(ent);
            }
        }
        Err(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_with_tagged_html_returns_stripped_text() {
        let html = b"<html><body><h1>Title</h1><p>Hello <b>world</b></p></body></html>";
        let got = extract(html).expect("extract");
        assert!(!got.contains("<h1>"), "{got:?}");
        assert!(!got.contains("<p>"), "{got:?}");
        assert!(!got.contains("<b>"), "{got:?}");
        assert!(got.contains("Title"), "{got:?}");
        assert!(got.contains("Hello"), "{got:?}");
        assert!(got.contains("world"), "{got:?}");
    }
}
