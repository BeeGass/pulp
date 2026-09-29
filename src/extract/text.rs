//! Plain-text extraction, including encoding detection for non-UTF-8 bytes.

use crate::error::Error;

const UTF8_BOM: &[u8] = &[0xEF, 0xBB, 0xBF];
const UTF16_LE_BOM: &[u8] = &[0xFF, 0xFE];
const UTF16_BE_BOM: &[u8] = &[0xFE, 0xFF];
const UTF32_LE_BOM: &[u8] = &[0xFF, 0xFE, 0x00, 0x00];
const UTF32_BE_BOM: &[u8] = &[0x00, 0x00, 0xFE, 0xFF];
/// Bytes examined when guessing whether NUL-bearing bytes are UTF-16 text.
const SNIFF_BYTES: usize = 8192;

/// A text encoding whose code units are wider than a byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WideText {
    Utf16Le,
    Utf16Be,
    Utf32Le,
    Utf32Be,
}

/// Recognize UTF-16 or UTF-32 text, returning the encoding and the length of
/// its byte order mark.
///
/// With a BOM, or without one when NUL bytes sit on one side of most code
/// units (the shape of mostly-Latin UTF-16), the bytes count as wide text
/// only if the first 8 KiB decode to printable text. Such files hold NUL
/// bytes, so without this check they would be taken for binary or dumped
/// with NULs in them; the printable test keeps a binary file that happens
/// to start with `FF FE` binary.
pub(crate) fn sniff_wide_text(bytes: &[u8]) -> Option<(WideText, usize)> {
    let (wide, bom) = wide_bom(bytes).or_else(|| guess_utf16(bytes))?;
    let body = &bytes[bom..];
    let sample = &body[..body.len().min(SNIFF_BYTES)];
    let text = decode_wide(sample, wide);
    let chars = text.chars().count();
    let odd = text
        .chars()
        .filter(|c| {
            *c == char::REPLACEMENT_CHARACTER
                || (c.is_control() && !matches!(c, '\t' | '\n' | '\r' | '\u{c}'))
        })
        .count();
    // A cut at the end of the sample can leave one partial code unit.
    (odd.saturating_sub(1) * 100 <= chars).then_some((wide, bom))
}

/// The encoding a UTF-16 or UTF-32 byte order mark names, and its length.
fn wide_bom(bytes: &[u8]) -> Option<(WideText, usize)> {
    if bytes.starts_with(UTF32_LE_BOM) {
        Some((WideText::Utf32Le, 4))
    } else if bytes.starts_with(UTF32_BE_BOM) {
        Some((WideText::Utf32Be, 4))
    } else if bytes.starts_with(UTF16_LE_BOM) {
        Some((WideText::Utf16Le, 2))
    } else if bytes.starts_with(UTF16_BE_BOM) {
        Some((WideText::Utf16Be, 2))
    } else {
        None
    }
}

/// UTF-16 byte order guessed from where the NUL bytes fall.
fn guess_utf16(bytes: &[u8]) -> Option<(WideText, usize)> {
    let sample = &bytes[..bytes.len().min(SNIFF_BYTES) & !1];
    if sample.len() < 4 || !sample.contains(&0) {
        return None;
    }
    let units = sample.len() / 2;
    let zero_even = sample.iter().step_by(2).filter(|b| **b == 0).count();
    let zero_odd = sample
        .iter()
        .skip(1)
        .step_by(2)
        .filter(|b| **b == 0)
        .count();
    // Mostly-ASCII UTF-16 has a zero high byte in most units and almost
    // never a zero low byte.
    let lopsided = |high: usize, low: usize| high * 10 >= units * 7 && low * 20 <= units;
    if lopsided(zero_odd, zero_even) {
        Some((WideText::Utf16Le, 0))
    } else if lopsided(zero_even, zero_odd) {
        Some((WideText::Utf16Be, 0))
    } else {
        None
    }
}

fn decode_wide(bytes: &[u8], wide: WideText) -> String {
    match wide {
        WideText::Utf16Le => encoding_rs::UTF_16LE
            .decode_without_bom_handling(bytes)
            .0
            .into_owned(),
        WideText::Utf16Be => encoding_rs::UTF_16BE
            .decode_without_bom_handling(bytes)
            .0
            .into_owned(),
        WideText::Utf32Le | WideText::Utf32Be => {
            let mut out = String::with_capacity(bytes.len() / 4);
            let (units, rest) = bytes.as_chunks::<4>();
            for &raw in units {
                let code = if wide == WideText::Utf32Le {
                    u32::from_le_bytes(raw)
                } else {
                    u32::from_be_bytes(raw)
                };
                out.push(char::from_u32(code).unwrap_or(char::REPLACEMENT_CHARACTER));
            }
            if !rest.is_empty() {
                out.push(char::REPLACEMENT_CHARACTER);
            }
            out
        }
    }
}

/// Decode file bytes to a string, guessing a legacy encoding when needed.
///
/// Empty input yields an empty string. A UTF-8 BOM is stripped. UTF-16 and
/// UTF-32 are decoded when a BOM marks them, and UTF-16 also when its NUL
/// pattern gives it away (see [`sniff_wide_text`]). Valid UTF-8 uses a fast
/// path so Rust and Lean sources are returned unchanged. Anything else goes
/// through chardetng plus encoding_rs.
#[must_use]
pub fn decode_bytes(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return String::new();
    }

    let bytes = bytes.strip_prefix(UTF8_BOM).unwrap_or(bytes);
    if bytes.is_empty() {
        return String::new();
    }

    // A BOM decides the encoding outright; without one, UTF-16 must look
    // like text before it is decoded as such.
    if let Some((wide, bom)) = wide_bom(bytes).or_else(|| sniff_wide_text(bytes)) {
        return decode_wide(&bytes[bom..], wide);
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

    fn utf16(text: &str, little: bool) -> Vec<u8> {
        text.encode_utf16()
            .flat_map(|unit| {
                if little {
                    unit.to_le_bytes()
                } else {
                    unit.to_be_bytes()
                }
            })
            .collect()
    }

    #[test]
    fn test_decode_bytes_with_utf16_without_bom_returns_text_without_nuls() {
        let text = "hello utf16\r\nsecond line, caf\u{e9}\n";
        assert_eq!(decode_bytes(&utf16(text, true)), text);
        assert_eq!(decode_bytes(&utf16(text, false)), text);
        assert_eq!(
            sniff_wide_text(&utf16(text, true)),
            Some((WideText::Utf16Le, 0))
        );
    }

    #[test]
    fn test_decode_bytes_with_utf32_bom_returns_text() {
        let text = "hello utf32 \u{1F600}\n";
        let mut le = UTF32_LE_BOM.to_vec();
        let mut be = UTF32_BE_BOM.to_vec();
        for c in text.chars() {
            le.extend_from_slice(&u32::from(c).to_le_bytes());
            be.extend_from_slice(&u32::from(c).to_be_bytes());
        }
        assert_eq!(decode_bytes(&le), text);
        assert_eq!(decode_bytes(&be), text);
    }

    #[test]
    fn test_sniff_wide_text_with_bom_on_binary_bytes_returns_none() {
        let mut binary = vec![0xFF, 0xFE];
        binary.extend((0..=255u8).cycle().take(4096));
        assert_eq!(sniff_wide_text(&binary), None);
        let mut text = vec![0xFF, 0xFE];
        text.extend("real text\n".encode_utf16().flat_map(u16::to_le_bytes));
        assert_eq!(sniff_wide_text(&text), Some((WideText::Utf16Le, 2)));
    }

    #[test]
    fn test_sniff_wide_text_with_binary_bytes_returns_none() {
        let binary: Vec<u8> = (0..=255u8).cycle().take(4096).collect();
        assert_eq!(sniff_wide_text(&binary), None);
        let small_u16s: Vec<u8> = (0..2048u16).flat_map(|n| (n % 40).to_le_bytes()).collect();
        assert_eq!(sniff_wide_text(&small_u16s), None);
        assert_eq!(sniff_wide_text(b"plain ascii text\n"), None);
    }
}
