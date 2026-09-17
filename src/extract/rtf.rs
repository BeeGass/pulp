use rtf_parser::RtfDocument;

/// Extract plain text from RTF. Unparseable input is returned as decoded bytes.
pub fn extract(bytes: &[u8]) -> Result<String, crate::error::Error> {
    let decoded = crate::extract::text::decode_bytes(bytes);
    match RtfDocument::try_from(decoded.as_str()) {
        Ok(doc) => Ok(doc.get_text()),
        Err(_) => Ok(decoded),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_with_simple_rtf_returns_plain_text() {
        let rtf = r"{\rtf1\ansi{\fonttbl\f0\fswiss Helvetica;}\f0\pard Hello {\b world}.\par }";
        let text = extract(rtf.as_bytes()).expect("rtf extract");
        assert_eq!(text, "Hello world.");
    }

    #[test]
    fn test_extract_with_invalid_rtf_returns_decoded_bytes() {
        let text = extract(b"not rtf at all").expect("fallback decode");
        assert_eq!(text, "not rtf at all");
    }
}
