//! JSON / JSONL pretty-printing for LLM-readable dumps.

use serde::de::IgnoredAny;

use crate::error::Error;

const MAX_PRETTY_BYTES: usize = 1024 * 1024;

/// Decode bytes, then pretty-print JSON when the payload is small enough.
///
/// Valid JSON up to 1 MiB is re-indented two spaces per level. Only the
/// whitespace between tokens changes: keys keep their order, duplicate keys
/// stay, and numbers and strings keep their exact text, so `1.10`, a 30-digit
/// ID, or `1e400` read as written. Larger valid JSON is returned as decoded
/// text. JSONL (several lines, the first one JSON) re-indents each line.
/// Anything else, including JSON nested more than 128 levels, is returned as
/// decoded text.
pub fn extract(bytes: &[u8]) -> Result<String, Error> {
    let decoded = super::text::decode_bytes(bytes);
    if decoded.len() > MAX_PRETTY_BYTES {
        return Ok(decoded);
    }
    if is_json(&decoded) {
        return Ok(reindent(&decoded));
    }
    if is_jsonl(&decoded) {
        return Ok(pretty_jsonl(&decoded));
    }
    Ok(decoded)
}

/// Whether `text` is exactly one JSON value. Numbers are not converted, so
/// out-of-range values such as `1e400` still count as JSON.
fn is_json(text: &str) -> bool {
    serde_json::from_str::<IgnoredAny>(text).is_ok()
}

fn is_jsonl(decoded: &str) -> bool {
    if decoded.lines().nth(1).is_none() {
        return false;
    }
    let Some(first) = decoded.lines().find(|line| !line.trim().is_empty()) else {
        return false;
    };
    is_json(first.trim())
}

fn pretty_jsonl(decoded: &str) -> String {
    let mut out = String::new();
    let mut first = true;
    for line in decoded.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if !first {
            out.push('\n');
        }
        first = false;
        if is_json(trimmed) {
            out.push_str(&reindent(trimmed));
        } else {
            out.push_str(line);
        }
    }
    out
}

/// Lay out valid JSON with a two-space indent, copying every string,
/// number, and literal token byte for byte.
///
/// The layout matches `serde_json::to_string_pretty`: one member per line,
/// `"key": value`, and `{}` / `[]` for empty containers.
fn reindent(src: &str) -> String {
    let bytes = src.as_bytes();
    let mut out = String::with_capacity(src.len() + src.len() / 2);
    let mut depth = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => {
                let start = i;
                i += 1;
                while i < bytes.len() {
                    match bytes[i] {
                        b'\\' => i += 2,
                        b'"' => {
                            i += 1;
                            break;
                        }
                        _ => i += 1,
                    }
                }
                out.push_str(&src[start..i.min(bytes.len())]);
                continue;
            }
            open @ (b'{' | b'[') => {
                let close = if open == b'{' { b'}' } else { b']' };
                let next = skip_ws(bytes, i + 1);
                out.push(char::from(open));
                if bytes.get(next) == Some(&close) {
                    out.push(char::from(close));
                    i = next + 1;
                    continue;
                }
                depth += 1;
                push_newline(&mut out, depth);
            }
            close @ (b'}' | b']') => {
                depth = depth.saturating_sub(1);
                push_newline(&mut out, depth);
                out.push(char::from(close));
            }
            b',' => {
                out.push(',');
                push_newline(&mut out, depth);
            }
            b':' => out.push_str(": "),
            b' ' | b'\t' | b'\n' | b'\r' => {}
            _ => {
                let start = i;
                while i < bytes.len() && !is_json_delimiter(bytes[i]) {
                    i += 1;
                }
                out.push_str(&src[start..i]);
                continue;
            }
        }
        i += 1;
    }
    out
}

fn skip_ws(bytes: &[u8], mut i: usize) -> usize {
    while i < bytes.len() && matches!(bytes[i], b' ' | b'\t' | b'\n' | b'\r') {
        i += 1;
    }
    i
}

fn is_json_delimiter(b: u8) -> bool {
    matches!(
        b,
        b',' | b':' | b'{' | b'}' | b'[' | b']' | b'"' | b' ' | b'\t' | b'\n' | b'\r'
    )
}

fn push_newline(out: &mut String, depth: usize) {
    out.push('\n');
    for _ in 0..depth {
        out.push_str("  ");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_with_compact_object_returns_pretty_json() {
        let input = br#"{"a":1,"b":true}"#;
        let got = extract(input).expect("extract");
        let expected =
            serde_json::to_string_pretty(&serde_json::json!({"a": 1, "b": true})).expect("pretty");
        assert_eq!(got, expected);
    }

    #[test]
    fn test_extract_with_oversize_json_returns_decoded_without_pretty() {
        let inner = "x".repeat(MAX_PRETTY_BYTES + 8);
        let input = format!("{{\"a\":\"{inner}\"}}");
        let got = extract(input.as_bytes()).expect("extract");
        assert_eq!(got, input);
        assert!(!got.contains('\n'));
    }

    #[test]
    fn test_extract_with_key_order_duplicates_and_long_numbers_keeps_them() {
        let input = br#"{"zeta":1,"alpha":{"b":[],"a":{}},"id":123456789012345678901234567890,"id":2,"f":1.10,"big":1e400,"s":"a, b: {c}"}"#;
        let got = extract(input).expect("extract");
        let expected = r#"{
  "zeta": 1,
  "alpha": {
    "b": [],
    "a": {}
  },
  "id": 123456789012345678901234567890,
  "id": 2,
  "f": 1.10,
  "big": 1e400,
  "s": "a, b: {c}"
}"#;
        assert_eq!(got, expected);
    }

    #[test]
    fn test_extract_with_values_matches_serde_json_layout() {
        // Keys already sorted, so serde_json's sorted map lays out the same.
        let input = br#" [ {"e": "q\"uote\\", "k" : [1, 2, {"x": null}]}, [], {}, "t", false ] "#;
        let got = extract(input).expect("extract");
        let value: serde_json::Value = serde_json::from_slice(input).unwrap();
        assert_eq!(got, serde_json::to_string_pretty(&value).unwrap());
    }

    #[test]
    fn test_extract_with_jsonl_keeps_each_line_in_order() {
        let got = extract(b"{\"b\":1,\"a\":2}\n{\"n\":1.50}\nnot json\n").expect("extract");
        assert_eq!(
            got,
            "{\n  \"b\": 1,\n  \"a\": 2\n}\n{\n  \"n\": 1.50\n}\nnot json"
        );
    }
}
