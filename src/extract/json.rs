//! JSON / JSONL pretty-printing for LLM-readable dumps.

use serde_json::Value;

use crate::error::Error;

const MAX_PRETTY_BYTES: usize = 1024 * 1024;

/// Decode bytes, then pretty-print JSON when the payload is small enough.
///
/// Objects/arrays at most 1 MiB are pretty-printed. Larger valid JSON is
/// returned as decoded text. JSONL (multiple lines, first line parses as JSON)
/// pretty-prints each line. Parse failure returns the decoded text.
pub fn extract(bytes: &[u8]) -> Result<String, Error> {
    let decoded = super::text::decode_bytes(bytes);

    match serde_json::from_str::<Value>(&decoded) {
        Ok(value) => {
            if decoded.len() <= MAX_PRETTY_BYTES {
                serde_json::to_string_pretty(&value).map_err(|e| Error::msg(e.to_string()))
            } else {
                Ok(decoded)
            }
        }
        Err(_) => {
            if decoded.len() <= MAX_PRETTY_BYTES && is_jsonl(&decoded) {
                Ok(pretty_jsonl(&decoded))
            } else {
                Ok(decoded)
            }
        }
    }
}

fn is_jsonl(decoded: &str) -> bool {
    if decoded.lines().nth(1).is_none() {
        return false;
    }
    let Some(first) = decoded.lines().find(|line| !line.trim().is_empty()) else {
        return false;
    };
    serde_json::from_str::<Value>(first.trim()).is_ok()
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
        match serde_json::from_str::<Value>(trimmed) {
            Ok(value) => match serde_json::to_string_pretty(&value) {
                Ok(pretty) => out.push_str(&pretty),
                Err(_) => out.push_str(line),
            },
            Err(_) => out.push_str(line),
        }
    }
    out
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
}
