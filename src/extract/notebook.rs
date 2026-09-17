//! Jupyter notebook (`.ipynb`) extraction.

use serde_json::Value;

use crate::error::Error;

/// Convert a Jupyter notebook to markdown-ish cell text.
///
/// Each cell is emitted as `## Cell {i} ({cell_type})` followed by its source
/// (string or joined array). When `include_outputs` is true, stream `text` and
/// `data["text/plain"]` are appended; `image/*` payloads are skipped.
pub fn extract(bytes: &[u8], include_outputs: bool) -> Result<String, Error> {
    let decoded = super::text::decode_bytes(bytes);
    let value: Value =
        serde_json::from_str(&decoded).map_err(|e| Error::msg(format!("notebook: {e}")))?;
    let cells = value
        .get("cells")
        .and_then(Value::as_array)
        .ok_or_else(|| Error::msg("notebook missing cells array"))?;

    let mut out = String::new();
    for (i, cell) in cells.iter().enumerate() {
        if i > 0 {
            if !out.ends_with('\n') {
                out.push('\n');
            }
            out.push('\n');
        }
        write_cell(&mut out, i, cell, include_outputs);
    }
    Ok(out)
}

fn write_cell(out: &mut String, i: usize, cell: &Value, include_outputs: bool) {
    let cell_type = cell
        .get("cell_type")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    out.push_str(&format!("## Cell {i} ({cell_type})\n"));

    if let Some(source) = cell.get("source").and_then(join_source) {
        out.push_str(&source);
        if !source.is_empty() && !source.ends_with('\n') {
            out.push('\n');
        }
    }

    if include_outputs {
        write_outputs(out, cell);
    }
}

fn write_outputs(out: &mut String, cell: &Value) {
    let Some(outputs) = cell.get("outputs").and_then(Value::as_array) else {
        return;
    };
    for output in outputs {
        if let Some(text) = output_text(output) {
            if !out.ends_with('\n') {
                out.push('\n');
            }
            out.push_str(&text);
            if !text.ends_with('\n') {
                out.push('\n');
            }
        }
    }
}

fn output_text(output: &Value) -> Option<String> {
    let obj = output.as_object()?;
    let mut chunks = Vec::new();
    if let Some(text) = obj.get("text").and_then(join_source) {
        if !text.is_empty() {
            chunks.push(text);
        }
    }
    if let Some(data) = obj.get("data").and_then(Value::as_object) {
        if let Some(plain) = data.get("text/plain").and_then(join_source) {
            if !plain.is_empty() {
                chunks.push(plain);
            }
        }
    }
    if chunks.is_empty() {
        None
    } else {
        Some(chunks.join(""))
    }
}

fn join_source(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Array(items) => {
            let mut s = String::new();
            for item in items {
                if let Value::String(part) = item {
                    s.push_str(part);
                }
            }
            Some(s)
        }
        _ => None,
    }
}
