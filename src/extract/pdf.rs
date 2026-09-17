/// Extract plain text from a PDF.
pub fn extract(bytes: &[u8]) -> Result<String, crate::error::Error> {
    pdf_extract::extract_text_from_mem(bytes)
        .map(|text| text.trim().to_string())
        .map_err(|err| crate::error::Error::msg(err.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hello_pdf(text: &str) -> Vec<u8> {
        let stream = format!("BT /F1 12 Tf 72 720 Td ({text}) Tj ET\n");
        let content = format!(
            "<< /Length {} >>\nstream\n{stream}endstream\n",
            stream.len()
        );
        let objs = [
            "<< /Type /Catalog /Pages 2 0 R >>\n".to_string(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>\n".to_string(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 4 0 R /Resources << /Font << /F1 5 0 R >> >> >>\n".to_string(),
            content,
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>\n".to_string(),
        ];

        let mut body = Vec::from(&b"%PDF-1.4\n"[..]);
        let mut offsets = vec![0usize];
        for (i, obj) in objs.iter().enumerate() {
            offsets.push(body.len());
            let n = i + 1;
            body.extend_from_slice(format!("{n} 0 obj\n{obj}endobj\n").as_bytes());
        }
        let xref_at = body.len();
        let mut xref = format!("xref\n0 {}\n", offsets.len());
        xref.push_str("0000000000 65535 f \n");
        for off in offsets.iter().skip(1) {
            xref.push_str(&format!("{off:010} 00000 n \n"));
        }
        xref.push_str(&format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref_at}\n%%EOF\n",
            offsets.len()
        ));
        body.extend_from_slice(xref.as_bytes());
        body
    }

    #[test]
    fn test_extract_with_simple_pdf_returns_trimmed_text() {
        let bytes = hello_pdf("Hello PDF");
        let text = extract(&bytes).expect("pdf extract");
        assert!(
            text.contains("Hello PDF"),
            "expected extracted text to contain 'Hello PDF', got {text:?}"
        );
        assert_eq!(text, text.trim());
    }

    #[test]
    fn test_extract_with_invalid_bytes_returns_error() {
        let err = extract(b"not a pdf").expect_err("invalid pdf");
        assert!(!err.to_string().is_empty());
    }
}
