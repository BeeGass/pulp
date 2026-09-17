//! XML to plain text via quick-xml events.

use quick_xml::events::Event;

use crate::error::Error;

/// Convert XML bytes to readable text.
///
/// Emits character data. A newline is inserted at the start of `p`, `para`,
/// `div`, `br`, `tr`, `li`, and `h1`–`h6`. Runs of more than two newlines are
/// collapsed. Parse failures pass through [`super::text::decode_bytes`].
pub fn extract(bytes: &[u8]) -> Result<String, Error> {
    let decoded = super::text::decode_bytes(bytes);
    match extract_xml(&decoded) {
        Some(text) => Ok(text),
        None => Ok(decoded),
    }
}

fn extract_xml(decoded: &str) -> Option<String> {
    let mut reader = quick_xml::Reader::from_str(decoded);
    let mut out = String::new();
    loop {
        match reader.read_event() {
            Ok(Event::Start(e) | Event::Empty(e)) => {
                if is_break_tag(e.local_name().into_inner()) {
                    out.push('\n');
                }
            }
            Ok(Event::Text(e)) => push_text(&mut out, e.as_ref()),
            Ok(Event::CData(e)) => out.push_str(e.as_ref()),
            Ok(Event::GeneralRef(r)) => push_entity(&mut out, &r),
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(_) => return None,
        }
    }
    Some(collapse_newlines(&out).trim().to_string())
}

fn is_break_tag(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "p" | "para" | "div" | "br" | "tr" | "li" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6"
    )
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

/// Collapse runs of more than two newlines to a blank line.
fn collapse_newlines(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut newline_run = 0usize;
    for c in s.chars() {
        if c == '\r' {
            continue;
        }
        if c == '\n' {
            newline_run += 1;
            if newline_run <= 2 {
                out.push('\n');
            }
        } else {
            newline_run = 0;
            out.push(c);
        }
    }
    out
}
