//! CSV / TSV extraction as markdown tables or tab-separated text.

use crate::error::Error;

const MAX_TABLE_COLS: usize = 24;
const MAX_TABLE_ROWS: usize = 2000;

/// Parse CSV/TSV bytes using `delimiter`.
///
/// The first row is treated as headers. Small tables (at most 24 columns and
/// 2000 data rows) become markdown. Larger ones become TSV-like text. Parse
/// failure passes through [`super::text::decode_bytes`].
pub fn extract(bytes: &[u8], delimiter: u8) -> Result<String, Error> {
    let decoded = super::text::decode_bytes(bytes);
    match parse_csv(&decoded, delimiter) {
        Some(text) => Ok(text),
        None => Ok(decoded),
    }
}

fn parse_csv(decoded: &str, delimiter: u8) -> Option<String> {
    let mut reader = ::csv::ReaderBuilder::new()
        .delimiter(delimiter)
        .has_headers(true)
        .from_reader(decoded.as_bytes());

    let headers = reader.headers().ok()?.clone();
    let mut rows = Vec::new();
    for record in reader.records() {
        rows.push(record.ok()?);
    }

    if headers.is_empty() && rows.is_empty() {
        return Some(String::new());
    }

    if headers.len() <= MAX_TABLE_COLS && rows.len() <= MAX_TABLE_ROWS {
        Some(markdown_table(&headers, &rows))
    } else {
        Some(tsv_like(&headers, &rows))
    }
}

fn markdown_table(headers: &::csv::StringRecord, rows: &[::csv::StringRecord]) -> String {
    let n = headers.len();
    let mut out = String::new();
    push_md_row(&mut out, headers.iter());
    out.push('|');
    for _ in 0..n {
        out.push_str(" --- |");
    }
    out.push('\n');
    for row in rows {
        out.push('|');
        for i in 0..n {
            out.push(' ');
            out.push_str(&escape_md_cell(row.get(i).unwrap_or("")));
            out.push_str(" |");
        }
        out.push('\n');
    }
    out
}

fn push_md_row<'a, I>(out: &mut String, cells: I)
where
    I: Iterator<Item = &'a str>,
{
    out.push('|');
    for cell in cells {
        out.push(' ');
        out.push_str(&escape_md_cell(cell));
        out.push_str(" |");
    }
    out.push('\n');
}

fn escape_md_cell(s: &str) -> String {
    s.replace('|', r"\|").replace('\r', "").replace('\n', " ")
}

fn tsv_like(headers: &::csv::StringRecord, rows: &[::csv::StringRecord]) -> String {
    let mut out = String::new();
    push_tsv_row(&mut out, headers);
    for row in rows {
        push_tsv_row(&mut out, row);
    }
    out
}

fn push_tsv_row(out: &mut String, rec: &::csv::StringRecord) {
    for (i, field) in rec.iter().enumerate() {
        if i > 0 {
            out.push('\t');
        }
        out.push_str(field);
    }
    out.push('\n');
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_with_small_csv_returns_markdown_table() {
        let input = b"name,age\nalice,30\nbob,31\n";
        let got = extract(input, b',').expect("extract");
        let expected = "\
| name | age |
| --- | --- |
| alice | 30 |
| bob | 31 |
";
        assert_eq!(got, expected);
    }
}
