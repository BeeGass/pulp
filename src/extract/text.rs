//! Plain-text extraction, including encoding detection for non-UTF-8 bytes.

use crate::error::Error;

const UTF8_BOM: &[u8] = &[0xEF, 0xBB, 0xBF];
const UTF16_LE_BOM: &[u8] = &[0xFF, 0xFE];
const UTF16_BE_BOM: &[u8] = &[0xFE, 0xFF];

/// Decode file bytes to a string, guessing a legacy encoding when needed.
///
/// Empty input yields an empty string. A UTF-8 BOM is stripped. UTF-16 LE/BE
/// BOMs are decoded via encoding_rs. Valid UTF-8 uses a fast path so Rust and
/// Lean sources are returned unchanged. Anything else goes through chardetng
/// plus encoding_rs.
#[must_use]
pub fn decode_bytes(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return String::new();
    }

    let bytes = bytes.strip_prefix(UTF8_BOM).unwrap_or(bytes);
    if bytes.is_empty() {
        return String::new();
    }

    if bytes.starts_with(UTF16_LE_BOM) {
        return encoding_rs::UTF_16LE.decode(bytes).0.into_owned();
    }
    if bytes.starts_with(UTF16_BE_BOM) {
        return encoding_rs::UTF_16BE.decode(bytes).0.into_owned();
    }

    if let Ok(text) = std::str::from_utf8(bytes) {
        return text.to_owned();
    }

    decode_legacy(bytes)
}

/// Extract text from a source file. Encoding is handled by [`decode_bytes`].
pub fn extract(bytes: &[u8]) -> Result<String, Error> {
    Ok(decode_bytes(bytes))
}

fn decode_legacy(bytes: &[u8]) -> String {
    let mut detector = chardetng::EncodingDetector::new(chardetng::Iso2022JpDetection::Allow);
    detector.feed(bytes, true);
    let encoding = detector.guess(None, chardetng::Utf8Detection::Allow);
    encoding.decode(bytes).0.into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_with_utf8_rust_snippet_returns_source() {
        let src = "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n";
        let got = extract(src.as_bytes()).expect("extract");
        assert_eq!(got, src);
    }

    #[test]
    fn test_extract_with_lean_snippet_returns_source() {
        let src = "theorem id (p : Prop) : p → p := fun h => h\n";
        let got = extract(src.as_bytes()).expect("extract");
        assert_eq!(got, src);
    }

    #[test]
    fn test_decode_bytes_with_empty_input_returns_empty() {
        assert_eq!(decode_bytes(b""), "");
    }

    #[test]
    fn test_decode_bytes_with_utf8_bom_returns_stripped_text() {
        let mut bytes = Vec::from(UTF8_BOM);
        bytes.extend_from_slice(b"fn main() {}");
        assert_eq!(decode_bytes(&bytes), "fn main() {}");
    }

    #[test]
    fn test_decode_bytes_with_utf16le_bom_returns_text() {
        let bytes = [0xFF, 0xFE, b'h', 0, b'i', 0];
        assert_eq!(decode_bytes(&bytes), "hi");
    }
}
