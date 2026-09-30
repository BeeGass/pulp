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
        pdf_with_page(&format!("BT /F1 12 Tf 72 720 Td ({text}) Tj ET\n"))
    }

    /// A one-page PDF whose page draws `stream`, with Helvetica as `/F1`.
    fn pdf_with_page(stream: &str) -> Vec<u8> {
        pdf_with_font(
            stream,
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>",
        )
    }

    /// A Type0 font over a predefined CMap `encoding`, with no ToUnicode map,
    /// as Japanese and Chinese PDFs often carry.
    fn cid_font(encoding: &str) -> String {
        format!(
            "<< /Type /Font /Subtype /Type0 /BaseFont /KozMinPr6N-Regular /Encoding /{encoding} \
             /DescendantFonts [<< /Type /Font /Subtype /CIDFontType0 /BaseFont /KozMinPr6N-Regular \
             /CIDSystemInfo << /Registry (Adobe) /Ordering (Japan1) /Supplement 6 >> \
             /FontDescriptor << /Type /FontDescriptor /FontName /KozMinPr6N-Regular /Flags 4 \
             /FontBBox [0 0 1000 1000] /ItalicAngle 0 /Ascent 880 /Descent -120 /CapHeight 700 \
             /StemV 80 >> >>] >>"
        )
    }

    /// A one-page PDF whose page draws `stream`, with `font` as `/F1`.
    fn pdf_with_font(stream: &str, font: &str) -> Vec<u8> {
        pdf_from_objects(&[
            "<< /Type /Catalog /Pages 2 0 R >>\n".to_string(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>\n".to_string(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 4 0 R /Resources << /Font << /F1 5 0 R >> >> >>\n".to_string(),
            stream_object("", stream),
            format!("{font}\n"),
        ])
    }

    /// A one-page PDF whose page draws `page`, with Helvetica as `/F1` and
    /// one XObject per entry of `xobjects`: `/X1` is the first, `/X2` the
    /// second, and so on. Each entry is its dictionary keys and its stream;
    /// every XObject can draw every other.
    fn pdf_with_xobjects(page: &str, xobjects: &[(&str, &str)]) -> Vec<u8> {
        let names: String = (0..xobjects.len())
            .map(|i| format!("/X{} {} 0 R ", i + 1, i + 6))
            .collect();
        let resources = format!("<< /Font << /F1 5 0 R >> /XObject << {names}>> >>");
        let mut objs = vec![
            "<< /Type /Catalog /Pages 2 0 R >>\n".to_string(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>\n".to_string(),
            format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 4 0 R /Resources {resources} >>\n"
            ),
            stream_object("", page),
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>\n".to_string(),
        ];
        for (keys, stream) in xobjects {
            objs.push(stream_object(
                &format!("/Type /XObject {keys} /Resources {resources} "),
                stream,
            ));
        }
        pdf_from_objects(&objs)
    }

    /// A stream object with the extra dictionary `keys`.
    fn stream_object(keys: &str, stream: &str) -> String {
        format!(
            "<< {keys}/Length {} >>\nstream\n{stream}endstream\n",
            stream.len()
        )
    }

    /// A PDF of `objs`, numbered from 1, with its cross-reference table.
    fn pdf_from_objects(objs: &[String]) -> Vec<u8> {
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
    fn test_extract_with_quote_operators_returns_every_line() {
        // `'` moves to the next line and shows a string; `"` also sets the
        // word and character spacing first. Ghostscript writes both.
        let bytes = pdf_with_page(
            "BT /F1 12 Tf 14 TL 72 720 Td (first line) Tj (second line) ' 1 0.5 (third line) \" ET\n",
        );
        let text = extract(&bytes).expect("pdf extract");
        let lines: Vec<&str> = text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect();
        assert_eq!(
            lines,
            ["first line", "second line", "third line"],
            "{text:?}"
        );
    }

    #[test]
    fn test_extract_with_ucs2_cmap_font_returns_its_text() {
        // こんにちは, as the UCS-2 codes the predefined CMap takes them for.
        let stream = "BT /F1 12 Tf 72 720 Td <30533093306B3061306F> Tj ET\n";
        let bytes = pdf_with_font(stream, &cid_font("UniJIS-UCS2-H"));
        let text = extract(&bytes).expect("pdf extract");
        assert_eq!(text, "\u{3053}\u{3093}\u{306b}\u{3061}\u{306f}");
    }

    #[test]
    fn test_extract_with_utf16_cmap_font_returns_text_beyond_the_bmp() {
        // 𠀋 (U+2000B) takes a surrogate pair; 日 does not.
        let stream = "BT /F1 12 Tf 72 720 Td <D840DC0B65E5> Tj ET\n";
        let bytes = pdf_with_font(stream, &cid_font("UniJIS-UTF16-H"));
        let text = extract(&bytes).expect("pdf extract");
        assert_eq!(text, "\u{2000b}\u{65e5}");
    }

    const FORM: &str = "/Subtype /Form /BBox [0 0 612 792]";

    #[test]
    fn test_extract_with_self_drawing_form_returns_its_text_once() {
        let bytes = pdf_with_xobjects(
            "BT /F1 12 Tf 72 720 Td (page) Tj ET /X1 Do\n",
            &[(FORM, "BT /F1 12 Tf 72 700 Td (inside) Tj ET /X1 Do\n")],
        );
        let text = extract(&bytes).expect("pdf extract");
        assert!(text.contains("page"), "{text:?}");
        assert_eq!(text.matches("inside").count(), 1, "{text:?}");
    }

    #[test]
    fn test_extract_with_forms_drawing_each_other_twice_stays_bounded() {
        // Each form draws the next one twice: 2^23 draws of the last form if
        // nothing bounds them.
        let chain: Vec<String> = (2..=24)
            .map(|next| format!("/X{next} Do /X{next} Do\n"))
            .chain(["BT /F1 12 Tf 72 700 Td (leaf) Tj ET\n".to_string()])
            .collect();
        let forms: Vec<(&str, &str)> = chain.iter().map(|s| (FORM, s.as_str())).collect();
        let bytes = pdf_with_xobjects("/X1 Do\n", &forms);
        let started = std::time::Instant::now();
        let text = extract(&bytes).expect("pdf extract");
        assert!(text.contains("leaf"), "{text:?}");
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
    }

    #[test]
    fn test_extract_with_image_xobject_returns_no_text_from_its_pixels() {
        let image =
            "/Subtype /Image /Width 26 /Height 1 /ColorSpace /DeviceGray /BitsPerComponent 8";
        let bytes = pdf_with_xobjects(
            "BT /F1 12 Tf 72 720 Td (caption) Tj ET /X1 Do\n",
            &[(image, "BT /F1 12 Tf (ghost) Tj ET\n")],
        );
        let text = extract(&bytes).expect("pdf extract");
        assert_eq!(text, "caption");
    }

    #[test]
    fn test_extract_with_invalid_bytes_returns_error() {
        let err = extract(b"not a pdf").expect_err("invalid pdf");
        assert!(!err.to_string().is_empty());
    }
}
