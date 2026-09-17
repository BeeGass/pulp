//! Browser bindings over pulp's extract/pack core (no filesystem).
//!
//! File bytes cross the JS↔WASM boundary as `Uint8Array` (not base64).

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use pulp::{
    classify, is_default_selected, language_label, pack_entries, Options, OutputFormat, Selection,
    TreeMode,
};
use serde::Serialize;
use serde::ser::Serialize as SerdeSerialize;
use wasm_bindgen::prelude::*;

#[wasm_bindgen(start)]
pub fn start() {
    console_error_panic_hook::set_once();
}

#[wasm_bindgen]
pub fn pulp_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}


fn to_js<T: serde::Serialize>(value: &T) -> Result<JsValue, JsValue> {
    let serializer = serde_wasm_bindgen::Serializer::new().serialize_maps_as_objects(true);
    SerdeSerialize::serialize(value, &serializer)
        .map_err(|e| JsValue::from_str(&e.to_string()))
}



fn js_now_ms() -> f64 {
    js_sys::Date::now()
}





fn normalize_rel(path: &str) -> String {
    path.replace('\\', "/")
        .trim_start_matches("./")
        .trim_start_matches('/')
        .to_string()
}

fn matches_exclude(relative: &str, patterns: &[String]) -> bool {
    if patterns.is_empty() {
        return false;
    }
    let name = relative.rsplit('/').next().unwrap_or(relative);
    for pat in patterns {
        let pat = pat.trim().trim_start_matches("**/");
        if pat.is_empty() {
            continue;
        }
        if relative.contains(pat.trim_end_matches("/**").trim_end_matches("/*")) {
            return true;
        }
        if name == pat || relative.ends_with(pat) {
            return true;
        }
        if let Some(prefix) = pat.strip_suffix('*') {
            if name.starts_with(prefix) || relative.contains(prefix) {
                return true;
            }
        }
    }
    false
}

#[derive(Serialize)]
struct ScanResponse {
    root: String,
    files: Vec<FileEntry>,
    file_count: usize,
    bytes: u64,
    truncated: bool,
    manifest_id: String,
}

#[derive(Serialize)]
struct FileEntry {
    id: String,
    relative: String,
    size: u64,
    kind: &'static str,
    language: &'static str,
    default_on: bool,
    oversized: bool,
}

#[derive(Serialize)]
struct PackResponse {
    dump: String,
    filename: String,
    format: String,
    files_extracted: usize,
    files_skipped: usize,
    tokens_est: usize,
    chars_emitted: usize,
    dump_bytes: usize,
    elapsed_ms: u128,
    truncated: bool,
    cancelled: bool,
    preview_truncated: bool,
    manifest_id: String,
    result_id: String,
    outcomes: Vec<FileOutcome>,
    error: Option<String>,
}

#[derive(Serialize)]
struct FileOutcome {
    id: String,
    relative: String,
    status: &'static str,
    message: String,
}


#[derive(serde::Deserialize)]
struct ScanFileIn {
    relative: String,
    #[serde(default)]
    size: u64,
}

#[derive(serde::Deserialize)]
struct ScanInput {
    #[serde(default)]
    files: Vec<ScanFileIn>,
    #[serde(default)]
    hidden: bool,
    #[serde(default)]
    archives: bool,
    #[serde(default)]
    exclude: Vec<String>,
    #[serde(default)]
    max_file_size: Option<u64>,
}

/// Classify a file list without reading contents (extension / name based).
#[wasm_bindgen]
pub fn scan_files(input: JsValue) -> Result<JsValue, JsValue> {
    let req: ScanInput = serde_wasm_bindgen::from_value(input)
        .map_err(|e| JsValue::from_str(&format!("invalid scan input: {e}")))?;
    let max_file = req.max_file_size.unwrap_or(8 * 1024 * 1024);

    let mut files = Vec::new();
    let mut bytes = 0u64;
    let mut truncated = false;
    let limit = 50_000usize;

    for f in req.files {
        if files.len() >= limit {
            truncated = true;
            break;
        }
        let relative = normalize_rel(&f.relative);
        if relative.is_empty() {
            continue;
        }
        if !req.hidden
            && relative
                .split('/')
                .any(|p| p.starts_with('.') && p != "." && p != "..")
        {
            continue;
        }
        if matches_exclude(&relative, &req.exclude) {
            continue;
        }
        let kind = classify(Path::new(&relative), None);
        let oversized = f.size > max_file;
        let default_on = is_default_selected(Path::new(&relative), kind)
            && !oversized
            && (!kind.is_archive() || req.archives);
        bytes = bytes.saturating_add(f.size);
        files.push(FileEntry {
            id: relative.clone(),
            relative: relative.clone(),
            size: f.size,
            kind: kind.as_str(),
            language: language_label(Path::new(&relative)),
            default_on,
            oversized,
        });
    }

    files.sort_by(|a, b| a.relative.cmp(&b.relative));
    let file_count = files.len();
    let resp = ScanResponse {
        root: "browser".into(),
        files,
        file_count,
        bytes,
        truncated,
        manifest_id: format!("m-{file_count}-{bytes}"),
    };
    to_js(&resp)
}

#[derive(serde::Deserialize)]
struct PackFileIn {
    relative: String,
    #[serde(default)]
    id: String,
    /// Browser `Uint8Array` (serde-wasm-bindgen maps this to `Vec<u8>`).
    bytes: Vec<u8>,
}

#[derive(serde::Deserialize)]
struct PackInput {
    #[serde(default)]
    files: Vec<PackFileIn>,
    #[serde(default)]
    selected: Vec<String>,
    #[serde(default)]
    format: String,
    #[serde(default)]
    archives: bool,
    #[serde(default)]
    binaries: bool,
    #[serde(default)]
    notebook_outputs: bool,
    #[serde(default)]
    source: bool,
    #[serde(default)]
    no_tree: bool,
    #[serde(default)]
    exclude: Vec<String>,
    #[serde(default)]
    max_file_size: Option<u64>,
    #[serde(default)]
    max_entries: Option<usize>,
}

/// Pack selected files. Each file is `{ relative, bytes: Uint8Array, id? }`.
#[wasm_bindgen]
pub fn pack_files(input: JsValue) -> Result<JsValue, JsValue> {
    let t0 = js_now_ms();
    let req: PackInput = serde_wasm_bindgen::from_value(input)
        .map_err(|e| JsValue::from_str(&format!("invalid pack input: {e}")))?;

    let selected_set: HashMap<String, ()> = req
        .selected
        .iter()
        .map(|s| (normalize_rel(s), ()))
        .collect();
    let filter_selected = !selected_set.is_empty();

    let mut entries: Vec<(String, Vec<u8>)> = Vec::new();
    let mut id_map: HashMap<String, String> = HashMap::new();

    for f in req.files {
        let relative = normalize_rel(&f.relative);
        if relative.is_empty() {
            continue;
        }
        let id = if f.id.is_empty() {
            relative.clone()
        } else {
            f.id.clone()
        };
        if filter_selected && !selected_set.contains_key(&relative) && !selected_set.contains_key(&id) {
            continue;
        }
        id_map.insert(relative.clone(), id);
        entries.push((relative, f.bytes));
    }

    if entries.is_empty() {
        let resp = PackResponse {
            dump: String::new(),
            filename: "pulp-browser.txt".into(),
            format: "txt".into(),
            files_extracted: 0,
            files_skipped: 0,
            tokens_est: 0,
            chars_emitted: 0,
            dump_bytes: 0,
            elapsed_ms: (js_now_ms() - t0).max(0.0) as u128,
            truncated: false,
            cancelled: false,
            preview_truncated: false,
            manifest_id: String::new(),
            result_id: String::new(),
            outcomes: Vec::new(),
            error: Some("No readable files in selection".into()),
        };
        return to_js(&resp);
    }

    let out_format = match req.format.to_ascii_lowercase().as_str() {
        "md" | "markdown" => OutputFormat::Markdown,
        "xml" => OutputFormat::Xml,
        _ => OutputFormat::Plain,
    };

    let selection = if filter_selected {
        Selection::Only(req.selected.iter().map(|s| normalize_rel(s)).collect())
    } else {
        Selection::AllEligible
    };

    let opts = Options {
        format: out_format,
        follow_archives: req.archives,
        skip_binaries: !req.binaries,
        notebook_outputs: req.notebook_outputs,
        source_mode: req.source,
        tree: if req.no_tree {
            TreeMode::None
        } else {
            TreeMode::Selected
        },
        exclude: req.exclude,
        max_file_size: req.max_file_size.unwrap_or(8 * 1024 * 1024),
        max_entries: req.max_entries.unwrap_or(5_000),
        selection,
        jobs: 1,
        ..Options::default()
    };

    let cancel = AtomicBool::new(false);
    let packed = pack_entries(&entries, &opts, Some(&cancel))
        .map_err(|e| JsValue::from_str(&format!("pack_entries failed: {e}")))?;

    let mut dump_buf = Vec::new();
    pulp::render::write_all(&mut dump_buf, &packed, &opts)
        .map_err(|e| JsValue::from_str(&format!("render failed: {e}")))?;
    let dump = String::from_utf8_lossy(&dump_buf).into_owned();
    let dump_bytes = dump.len();
    let preview_truncated = dump_bytes > 2_000_000;
    let dump_out = if preview_truncated {
        let mut s = dump.chars().take(2_000_000).collect::<String>();
        s.push_str("\n\n… preview truncated …\n");
        s
    } else {
        dump
    };

    let ext = out_format.extension();
    let filename = format!("pulp-browser.{ext}");

    let outcomes: Vec<FileOutcome> = packed
        .files
        .iter()
        .map(|f| FileOutcome {
            id: id_map
                .get(&f.relative)
                .cloned()
                .unwrap_or_else(|| f.relative.clone()),
            relative: f.relative.clone(),
            status: f.status.as_str(),
            message: f.status.message(f.size),
        })
        .collect();

    let error = if packed.stats.files_extracted == 0 {
        let sample = outcomes
            .iter()
            .take(5)
            .map(|o| format!("{} ({})", o.relative, o.status))
            .collect::<Vec<_>>()
            .join(", ");
        Some(format!(
            "Nothing extracted. Check size limits / binaries / archives. Sample: {sample}",
        ))
    } else {
        None
    };

    let resp = PackResponse {
        dump: dump_out,
        filename,
        format: ext.into(),
        files_extracted: packed.stats.files_extracted,
        files_skipped: packed.stats.files_skipped,
        tokens_est: packed.stats.tokens_est,
        chars_emitted: packed.stats.chars_emitted,
        dump_bytes,
        elapsed_ms: (js_now_ms() - t0).max(0.0) as u128,
        truncated: packed.stats.truncated,
        cancelled: packed.stats.cancelled || cancel.load(Ordering::Relaxed),
        preview_truncated,
        manifest_id: String::new(),
        result_id: format!("r-{}", packed.stats.files_extracted),
        outcomes,
        error,
    };
    to_js(&resp)
}

/// Tiny self-check used by the mill on load (pure Rust, no JS byte marshaling).
#[wasm_bindgen]
pub fn smoke_pack() -> Result<JsValue, JsValue> {
    let entries = vec![("hello.rs".to_string(), b"fn main() {}
".to_vec())];
    let opts = Options {
        format: OutputFormat::Plain,
        tree: TreeMode::None,
        jobs: 1,
        max_file_size: 8 * 1024 * 1024,
        max_entries: 100,
        ..Options::default()
    };
    let packed = pack_entries(&entries, &opts, None)
        .map_err(|e| JsValue::from_str(&format!("smoke pack_entries: {e}")))?;
    let mut dump_buf = Vec::new();
    pulp::render::write_all(&mut dump_buf, &packed, &opts)
        .map_err(|e| JsValue::from_str(&format!("smoke render: {e}")))?;
    let dump = String::from_utf8_lossy(&dump_buf).into_owned();
    let dump_bytes = dump.len();
    let resp = PackResponse {
        dump,
        filename: "smoke.txt".into(),
        format: "txt".into(),
        files_extracted: packed.stats.files_extracted,
        files_skipped: packed.stats.files_skipped,
        tokens_est: packed.stats.tokens_est,
        chars_emitted: packed.stats.chars_emitted,
        dump_bytes,
        elapsed_ms: 0,
        truncated: false,
        cancelled: false,
        preview_truncated: false,
        manifest_id: String::new(),
        result_id: "smoke".into(),
        outcomes: Vec::new(),
        error: None,
    };
    to_js(&resp)
}
