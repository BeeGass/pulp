//! Browser bindings over pulp's extract/pack core (no filesystem).
//!
//! File bytes cross the JS↔WASM boundary as `Uint8Array` (not base64).
//! Each instance keeps the extracted files of its last few packs, so the mill
//! can redraw a dump in another format without extracting again.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use pulp::{
    classify, default_exclude_globs, is_default_selected, language_name, pack_entries, FileStatus,
    MemoryFile, Options, OutputFormat, Packed, PackedFile, PathPolicy, Selection, TreeMode,
};
use serde::ser::Serialize as SerdeSerialize;
use serde::Serialize;
use wasm_bindgen::prelude::*;

const DEFAULT_MAX_FILE_SIZE: u64 = 8 * 1024 * 1024;
/// Longest dump returned inline. Copy and download fetch the rest.
const DUMP_PREVIEW_BYTES: usize = 2_000_000;
/// Longest text returned by [`preview_file`].
const FILE_PREVIEW_BYTES: usize = 32 * 1024;
/// Pack results each instance keeps for redraws and full dumps.
const MAX_RESULTS: usize = 4;
const RESULT_GONE: &str = "the dump is no longer in memory; pulp again";

#[wasm_bindgen(start)]
pub fn start() {
    std::panic::set_hook(Box::new(|info| {
        console_error_panic_hook::hook(info);
        // panic=abort turns the trap into the JS message "unreachable".
        // Throwing here keeps the panic text on the error the mill shows.
        wasm_bindgen::throw_str(&info.to_string());
    }));
}

#[wasm_bindgen]
pub fn pulp_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

fn to_js<T: serde::Serialize>(value: &T) -> Result<JsValue, JsValue> {
    let serializer = serde_wasm_bindgen::Serializer::new().serialize_maps_as_objects(true);
    SerdeSerialize::serialize(value, &serializer).map_err(|e| JsValue::from_str(&e.to_string()))
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

fn resolved_exclude(extra: Vec<String>) -> Vec<String> {
    if extra.is_empty() {
        default_exclude_globs()
    } else {
        extra
    }
}

/// The folder a folder pick named every path after (`tides` for
/// `tides/src/lib.rs`), or `.` when the paths share no folder.
fn grant_root(relatives: &[String]) -> String {
    pulp::tree::split_grant_root(relatives).0
}

/// `relative` as seen from inside the grant folder `root`. The local mill walks
/// from inside the folder it is given, so its filters never see the folder's
/// own name and its dump names files from there.
fn under_root<'a>(relative: &'a str, root: &str) -> &'a str {
    if root == "." {
        return relative;
    }
    relative
        .strip_prefix(root)
        .and_then(|rest| rest.strip_prefix('/'))
        .unwrap_or(relative)
}

/// Dump format from a name such as `txt`, `md`, or `xml`. Anything else is plain text.
fn parse_format(name: &str) -> OutputFormat {
    OutputFormat::from_ext(name).unwrap_or(OutputFormat::Plain)
}

/// Cut `text` to at most `max` bytes without splitting a character.
fn cap_text(mut text: String, max: usize) -> (String, bool) {
    if text.len() <= max {
        return (text, false);
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    (text, true)
}

thread_local! {
    static RESULTS: RefCell<ResultStore> = const { RefCell::new(ResultStore::new()) };
}

/// Extracted files from one pack, kept so the dump can be drawn again.
struct StoredResult {
    /// Files, stats, and directory map as the pack made them. The map is
    /// always built; [`StoredResult::render`] leaves it out when it is off.
    packed: Packed,
    /// Format and map setting the pack asked for. [`artifact`] renders these.
    format: OutputFormat,
    tree: bool,
    /// Markdown labels HTML and XML fences with their language only in source mode.
    source_mode: bool,
    /// Time the pack took to extract. A redraw reports it again, as `pulp ui` does.
    extract_ms: f64,
}

impl StoredResult {
    /// The whole dump in `format`, with or without the directory map.
    fn render(&mut self, format: OutputFormat, tree: bool) -> Result<String, String> {
        let opts = Options {
            format,
            source_mode: self.source_mode,
            ..Options::default()
        };
        // The writers print whatever map `packed.tree` holds, so lift it out while it is off.
        let map = (!tree).then(|| std::mem::take(&mut self.packed.tree));
        let mut buf = Vec::new();
        let written = pulp::render::write_all(&mut buf, &self.packed, &opts);
        if let Some(map) = map {
            self.packed.tree = map;
        }
        written.map_err(|e| format!("render failed: {e}"))?;
        String::from_utf8(buf).map_err(|e| format!("render failed: {e}"))
    }

    /// Characters and estimated tokens, as a fresh pack with this map setting counts them.
    fn counts(&self, tree: bool) -> (usize, usize) {
        let stats = &self.packed.stats;
        if tree {
            return (stats.chars_emitted, stats.tokens_est);
        }
        pulp::tokens::summarize_chunks(
            self.packed
                .files
                .iter()
                .filter(|file| file.status == FileStatus::Extracted)
                .map(|file| file.text.as_str()),
        )
    }
}

/// The last [`MAX_RESULTS`] pack results, least recently used first.
struct ResultStore {
    entries: VecDeque<(String, StoredResult)>,
}

impl ResultStore {
    const fn new() -> Self {
        Self {
            entries: VecDeque::new(),
        }
    }

    /// Keep `result` as the newest entry, dropping the least recently used past the cap.
    fn put(&mut self, id: String, result: StoredResult) -> &mut StoredResult {
        self.remove(&id);
        while self.entries.len() >= MAX_RESULTS {
            self.entries.pop_front();
        }
        self.entries.push_back((id, result));
        let last = self.entries.len() - 1;
        &mut self.entries[last].1
    }

    /// The result stored under `id`, now the most recently used.
    fn get(&mut self, id: &str) -> Option<&mut StoredResult> {
        let at = self.entries.iter().position(|(key, _)| key == id)?;
        let entry = self.entries.remove(at)?;
        self.entries.push_back(entry);
        self.entries.back_mut().map(|(_, result)| result)
    }

    /// Forget `id`. Returns whether it was stored.
    fn remove(&mut self, id: &str) -> bool {
        let before = self.entries.len();
        self.entries.retain(|(key, _)| key != id);
        self.entries.len() != before
    }
}

/// Run `f` on the stored result `result_id`.
fn with_result<T>(
    result_id: &str,
    f: impl FnOnce(&mut StoredResult) -> Result<T, String>,
) -> Result<T, JsValue> {
    RESULTS
        .with(|results| {
            let mut results = results.borrow_mut();
            let stored = results
                .get(result_id)
                .ok_or_else(|| RESULT_GONE.to_string())?;
            f(stored)
        })
        .map_err(|e| JsValue::from_str(&e))
}

/// Full dump for a previous [`pack_files`] result, in the format it was packed in.
#[wasm_bindgen]
pub fn artifact(result_id: &str) -> Result<String, JsValue> {
    with_result(result_id, |stored| {
        stored.render(stored.format, stored.tree)
    })
}

fn new_handle() -> String {
    let mut buf = [0u8; 8];
    let _ = getrandom::fill(&mut buf);
    buf.iter().fold(String::from("w-"), |mut s, b| {
        s.push_str(&format!("{b:02x}"));
        s
    })
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
    language: String,
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
    kind: &'static str,
    language: String,
    size: u64,
    message: String,
}

#[derive(Serialize)]
struct PreviewResponse {
    text: String,
    truncated: bool,
    status: &'static str,
    message: String,
    kind: &'static str,
    language: String,
}

#[derive(serde::Deserialize)]
struct ScanFileIn {
    relative: String,
    #[serde(default)]
    size: u64,
    /// Optional leading bytes so unknown extensions can be sniffed.
    #[serde(default)]
    head: Vec<u8>,
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
    let resp = scan(req).map_err(|e| JsValue::from_str(&e))?;
    to_js(&resp)
}

/// The files a grant offers for packing, filtered as the local mill filters a
/// walk from inside the granted folder.
fn scan(req: ScanInput) -> Result<ScanResponse, String> {
    let max_file = req.max_file_size.unwrap_or(DEFAULT_MAX_FILE_SIZE);
    let opts = Options {
        hidden: req.hidden,
        follow_archives: req.archives,
        exclude: resolved_exclude(req.exclude),
        max_file_size: max_file,
        ..Options::default()
    };
    let policy = PathPolicy::from_options(&opts).map_err(|e| e.to_string())?;

    let mut files = Vec::new();
    let mut bytes = 0u64;
    let truncated = false;
    let relatives: Vec<String> = req
        .files
        .iter()
        .map(|f| normalize_rel(&f.relative))
        .collect();
    let root = grant_root(&relatives);

    for (f, relative) in req.files.into_iter().zip(relatives) {
        if relative.is_empty() {
            continue;
        }
        if !policy.keep_walk(under_root(&relative, &root)) {
            continue;
        }
        let sniff = if f.head.is_empty() {
            None
        } else {
            Some(f.head.as_slice())
        };
        let kind = classify(Path::new(&relative), sniff);
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
            language: language_name(Path::new(&relative), kind),
            default_on,
            oversized,
        });
    }

    files.sort_by(|a, b| a.relative.cmp(&b.relative));
    let file_count = files.len();
    Ok(ScanResponse {
        root: "browser".into(),
        files,
        file_count,
        bytes,
        truncated,
        manifest_id: new_handle(),
    })
}

#[derive(serde::Deserialize)]
struct PackFileIn {
    relative: String,
    #[serde(default)]
    id: String,
    /// Browser `Uint8Array` (serde-wasm-bindgen maps this to `Vec<u8>`).
    bytes: Vec<u8>,
}

#[derive(Default, serde::Deserialize)]
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
    hidden: bool,
    #[serde(default)]
    no_tree: bool,
    #[serde(default)]
    exclude: Vec<String>,
    #[serde(default)]
    max_file_size: Option<u64>,
    #[serde(default)]
    max_entries: Option<usize>,
}

#[derive(serde::Deserialize)]
struct RenderInput {
    result_id: String,
    #[serde(default)]
    format: String,
    #[serde(default)]
    no_tree: bool,
}

/// One file for [`preview_file`], with the pack settings that decide its text.
#[derive(Default, serde::Deserialize)]
struct PreviewInput {
    relative: String,
    bytes: Vec<u8>,
    #[serde(default)]
    archives: bool,
    #[serde(default)]
    binaries: bool,
    #[serde(default)]
    notebook_outputs: bool,
    #[serde(default)]
    source: bool,
    #[serde(default)]
    hidden: bool,
    #[serde(default)]
    exclude: Vec<String>,
    #[serde(default)]
    max_file_size: Option<u64>,
}

#[derive(serde::Deserialize)]
struct TreeInput {
    #[serde(default)]
    paths: Vec<String>,
    /// Explicit diagram root. Empty uses [`pulp::tree::split_grant_root`].
    #[serde(default)]
    root: String,
    #[serde(default)]
    format: String,
}

/// Directory map for `paths` in the dump format. Reads no file bytes.
#[wasm_bindgen]
pub fn format_tree(input: JsValue) -> Result<String, JsValue> {
    let req: TreeInput = serde_wasm_bindgen::from_value(input)
        .map_err(|e| JsValue::from_str(&format!("invalid tree input: {e}")))?;
    let format = if req.format.trim().is_empty() {
        OutputFormat::Xml
    } else {
        OutputFormat::from_ext(&req.format)
            .ok_or_else(|| JsValue::from_str(&format!("unknown format {}", req.format)))?
    };
    let (root, paths) = if req.root.trim().is_empty() {
        pulp::tree::split_grant_root(&req.paths)
    } else {
        let paths = req
            .paths
            .iter()
            .map(|path| normalize_rel(path))
            .filter(|path| !path.is_empty())
            .collect();
        (req.root, paths)
    };
    if paths.is_empty() {
        return Err(JsValue::from_str("tick at least one file"));
    }
    pulp::render::format_directory_map(&root, &paths, format)
        .map_err(|e| JsValue::from_str(&format!("format tree failed: {e}")))
}

/// Extract the ticked files into a result that can be drawn in any format.
///
/// A folder pick names every path after the folder. The pack runs from inside
/// it, as `pulp ui` packs a folder: the dump names `src/lib.rs` under a
/// `tides/` map. Ids keep the full path, so outcomes still match the scan.
fn extract(req: PackInput) -> Result<StoredResult, String> {
    let files: Vec<PackFileIn> = req
        .files
        .into_iter()
        .filter_map(|mut f| {
            f.relative = normalize_rel(&f.relative);
            if f.relative.is_empty() {
                return None;
            }
            if f.id.is_empty() {
                f.id = f.relative.clone();
            }
            Some(f)
        })
        .collect();
    let relatives: Vec<String> = files.iter().map(|f| f.relative.clone()).collect();
    let root = grant_root(&relatives);

    let format = parse_format(&req.format);
    let opts = Options {
        roots: vec![PathBuf::from(&root)],
        format,
        follow_archives: req.archives,
        skip_binaries: !req.binaries,
        notebook_outputs: req.notebook_outputs,
        source_mode: req.source,
        hidden: req.hidden,
        // Always map the tree so a redraw can show it; rendering leaves it out when off.
        tree: TreeMode::Selected,
        exclude: resolved_exclude(req.exclude),
        max_file_size: req.max_file_size.unwrap_or(DEFAULT_MAX_FILE_SIZE),
        max_entries: req.max_entries.unwrap_or(0),
        selection: Selection::Only(req.selected.iter().map(|s| normalize_rel(s)).collect()),
        jobs: 1,
        ..Options::default()
    };
    let mem: Vec<MemoryFile<'_>> = files
        .iter()
        .map(|f| MemoryFile {
            id: &f.id,
            relative: under_root(&f.relative, &root),
            bytes: &f.bytes,
        })
        .collect();

    let packed =
        pack_entries(&mem, &opts, None).map_err(|e| format!("pack_entries failed: {e}"))?;
    Ok(StoredResult {
        packed,
        format,
        tree: !req.no_tree,
        source_mode: req.source,
        extract_ms: 0.0,
    })
}

/// A stored result drawn in `format` as the mill shows it: the dump capped
/// for display, stats, and per-file outcomes. `elapsed_ms` is left at zero.
fn respond(
    stored: &mut StoredResult,
    result_id: &str,
    format: OutputFormat,
    tree: bool,
) -> Result<PackResponse, String> {
    let dump = stored.render(format, tree)?;
    let dump_bytes = dump.len();
    let (mut dump, preview_truncated) = cap_text(dump, DUMP_PREVIEW_BYTES);
    if preview_truncated {
        dump.push_str("\n\n… preview truncated …\n");
    }
    let (chars_emitted, tokens_est) = stored.counts(tree);
    let outcomes = outcomes(&stored.packed.files);
    let stats = &stored.packed.stats;

    let error = if stats.files_extracted == 0 {
        let sample = outcomes
            .iter()
            .take(5)
            .map(|o| format!("{} [{}]: {}", o.relative, o.status, o.message))
            .collect::<Vec<_>>()
            .join(", ");
        Some(format!(
            "Nothing extracted. Check size limits / binaries / archives. Sample: {sample}",
        ))
    } else {
        None
    };

    let ext = format.extension();
    Ok(PackResponse {
        dump,
        filename: format!("pulp-browser.{ext}"),
        format: ext.into(),
        files_extracted: stats.files_extracted,
        files_skipped: stats.files_skipped,
        tokens_est,
        chars_emitted,
        dump_bytes,
        elapsed_ms: 0,
        truncated: stats.truncated,
        cancelled: stats.cancelled,
        preview_truncated,
        manifest_id: String::new(),
        result_id: result_id.to_string(),
        outcomes,
        error,
    })
}

fn outcomes(files: &[PackedFile]) -> Vec<FileOutcome> {
    files
        .iter()
        .map(|f| FileOutcome {
            id: f.id.clone(),
            relative: f.relative.clone(),
            status: f.status.as_str(),
            kind: f.kind.as_str(),
            language: language_name(Path::new(&f.relative), f.kind),
            size: f.size,
            message: f.status.message(f.size),
        })
        .collect()
}

/// Pack selected files. Each file is `{ relative, bytes: Uint8Array, id? }`.
///
/// The extracted files stay in this instance under `result_id`, so
/// [`render_result`] and [`artifact_as`] can draw them again.
#[wasm_bindgen]
pub fn pack_files(input: JsValue) -> Result<JsValue, JsValue> {
    let t0 = js_now_ms();
    let req: PackInput = serde_wasm_bindgen::from_value(input)
        .map_err(|e| JsValue::from_str(&format!("invalid pack input: {e}")))?;
    let mut stored = extract(req).map_err(|e| JsValue::from_str(&e))?;
    stored.extract_ms = js_now_ms() - t0;
    let result_id = new_handle();
    let mut resp = RESULTS
        .with(|results| {
            let mut results = results.borrow_mut();
            let stored = results.put(result_id.clone(), stored);
            let (format, tree) = (stored.format, stored.tree);
            respond(stored, &result_id, format, tree)
        })
        .map_err(|e| JsValue::from_str(&e))?;
    resp.elapsed_ms = (js_now_ms() - t0).max(0.0) as u128;
    to_js(&resp)
}

/// Redraw a stored result, `{ result_id, format, no_tree }`, without extracting
/// again. Returns what [`pack_files`] returns, under the same `result_id`.
#[wasm_bindgen]
pub fn render_result(input: JsValue) -> Result<JsValue, JsValue> {
    let t0 = js_now_ms();
    let req: RenderInput = serde_wasm_bindgen::from_value(input)
        .map_err(|e| JsValue::from_str(&format!("invalid render input: {e}")))?;
    let format = parse_format(&req.format);
    let resp = with_result(&req.result_id, |stored| {
        let mut resp = respond(stored, &req.result_id, format, !req.no_tree)?;
        resp.elapsed_ms = (stored.extract_ms + js_now_ms() - t0).max(0.0) as u128;
        Ok(resp)
    })?;
    to_js(&resp)
}

/// Full dump for a stored result, `{ result_id, format, no_tree }`.
#[wasm_bindgen]
pub fn artifact_as(input: JsValue) -> Result<String, JsValue> {
    let req: RenderInput = serde_wasm_bindgen::from_value(input)
        .map_err(|e| JsValue::from_str(&format!("invalid artifact input: {e}")))?;
    with_result(&req.result_id, |stored| {
        stored.render(parse_format(&req.format), !req.no_tree)
    })
}

/// Forget a stored result. Returns whether it was still held.
#[wasm_bindgen]
pub fn drop_result(result_id: &str) -> bool {
    RESULTS.with(|results| results.borrow_mut().remove(result_id))
}

/// The text pulp extracts from one file, capped for display. Stores nothing.
fn preview(req: PreviewInput) -> Result<PreviewResponse, String> {
    let relative = normalize_rel(&req.relative);
    if relative.is_empty() {
        return Err("preview needs a file path".into());
    }
    let kind = classify(Path::new(&relative), Some(&req.bytes));
    let language = language_name(Path::new(&relative), kind);
    let mut stored = extract(PackInput {
        files: vec![PackFileIn {
            relative: relative.clone(),
            id: String::new(),
            bytes: req.bytes,
        }],
        selected: vec![relative.clone()],
        archives: req.archives,
        binaries: req.binaries,
        notebook_outputs: req.notebook_outputs,
        source: req.source,
        hidden: req.hidden,
        exclude: req.exclude,
        max_file_size: req.max_file_size,
        ..PackInput::default()
    })?;

    let files = &mut stored.packed.files;
    let (text, status, message) = if files.is_empty() {
        // Hidden, excluded, or an archive with no member that would be packed.
        (
            String::new(),
            FileStatus::Extracted.as_str(),
            "Nothing in this file goes into the dump.".to_string(),
        )
    } else if files.len() == 1 && files[0].id == relative {
        // Ids keep the full path; the relative path starts inside the grant folder.
        let file = files.swap_remove(0);
        let message = file.status.message(file.size);
        let status = file.status.as_str();
        let text = if file.status == FileStatus::Extracted {
            file.text
        } else {
            String::new()
        };
        (text, status, message)
    } else {
        // An expanded archive: show its members the way a plain-text dump does.
        let (status, message) = if files.iter().any(|f| f.status == FileStatus::Extracted) {
            (FileStatus::Extracted.as_str(), String::new())
        } else {
            (
                files[0].status.as_str(),
                files[0].status.message(files[0].size),
            )
        };
        (stored.render(OutputFormat::Plain, false)?, status, message)
    };

    let (text, truncated) = cap_text(text, FILE_PREVIEW_BYTES);
    Ok(PreviewResponse {
        text,
        truncated,
        status,
        message,
        kind: kind.as_str(),
        language,
    })
}

/// Text pulp extracts from one file, `{ relative, bytes: Uint8Array }` plus the
/// pack settings that change it (`source`, `notebook_outputs`, `archives`,
/// `hidden`, ...), capped at 32 KiB. Stores nothing.
#[wasm_bindgen]
pub fn preview_file(input: JsValue) -> Result<JsValue, JsValue> {
    let req: PreviewInput = serde_wasm_bindgen::from_value(input)
        .map_err(|e| JsValue::from_str(&format!("invalid preview input: {e}")))?;
    let resp = preview(req).map_err(|e| JsValue::from_str(&e))?;
    to_js(&resp)
}

/// Tiny self-check used by the mill on load (pure Rust, no JS byte marshaling).
#[wasm_bindgen]
pub fn smoke_pack() -> Result<JsValue, JsValue> {
    let bytes = b"fn main() {}\n".to_vec();
    let entries = [MemoryFile {
        id: "hello.rs",
        relative: "hello.rs",
        bytes: &bytes,
    }];
    let opts = Options {
        format: OutputFormat::Plain,
        tree: TreeMode::None,
        jobs: 1,
        max_file_size: DEFAULT_MAX_FILE_SIZE,
        max_entries: 0,
        selection: Selection::AllEligible,
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

#[cfg(test)]
mod tests {
    use super::*;

    const FORMATS: [OutputFormat; 3] = [
        OutputFormat::Plain,
        OutputFormat::Markdown,
        OutputFormat::Xml,
    ];

    fn file(relative: &str, body: &[u8]) -> PackFileIn {
        PackFileIn {
            relative: relative.to_string(),
            id: String::new(),
            bytes: body.to_vec(),
        }
    }

    /// A small grant: Rust, Markdown, HTML (fenced differently in source
    /// mode), and a binary that is skipped and left off the map.
    fn project() -> Vec<PackFileIn> {
        vec![
            file("tides/src/lib.rs", b"pub fn tide() -> u32 {\n    2\n}\n"),
            file("tides/README.md", b"# Tides\n\nTwo a day. ```fence```\n"),
            file(
                "tides/site/index.html",
                b"<html><body><p>High water &amp; low</p></body></html>\n",
            ),
            file("tides/assets/logo.png", &[0, 159, 146, 150, 0, 1, 2, 3]),
        ]
    }

    fn input(files: Vec<PackFileIn>, format: &str, no_tree: bool) -> PackInput {
        PackInput {
            selected: files.iter().map(|f| f.relative.clone()).collect(),
            files,
            format: format.to_string(),
            no_tree,
            ..PackInput::default()
        }
    }

    /// One direct pack of the `tides` folder in `format`, from inside the
    /// folder as `pulp ui` packs it. A redraw must match it.
    fn fresh(format: OutputFormat, tree: bool, source: bool) -> (String, pulp::Stats) {
        let files = project();
        let mem: Vec<MemoryFile<'_>> = files
            .iter()
            .map(|f| MemoryFile {
                id: &f.relative,
                relative: f.relative.strip_prefix("tides/").unwrap(),
                bytes: &f.bytes,
            })
            .collect();
        let opts = Options {
            roots: vec![PathBuf::from("tides")],
            format,
            source_mode: source,
            tree: if tree {
                TreeMode::Selected
            } else {
                TreeMode::None
            },
            selection: Selection::Only(files.iter().map(|f| f.relative.clone()).collect()),
            jobs: 1,
            ..Options::default()
        };
        let packed = pack_entries(&mem, &opts, None).unwrap();
        let mut buf = Vec::new();
        pulp::render::write_all(&mut buf, &packed, &opts).unwrap();
        (String::from_utf8(buf).unwrap(), packed.stats)
    }

    fn stored_one() -> StoredResult {
        extract(input(vec![file("a.rs", b"fn a() {}\n")], "txt", false)).unwrap()
    }

    fn preview_of(relative: &str, body: &[u8]) -> PreviewInput {
        PreviewInput {
            relative: relative.to_string(),
            bytes: body.to_vec(),
            ..PreviewInput::default()
        }
    }

    /// Bytes of one file in the mill's built-in sample, `web/sample.json`.
    fn sample_file(path: &str) -> Vec<u8> {
        let raw = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../web/sample.json"
        ))
        .expect("read web/sample.json");
        let bundle: serde_json::Value = serde_json::from_str(&raw).expect("parse web/sample.json");
        let entry = bundle["files"]
            .as_array()
            .expect("sample files")
            .iter()
            .find(|f| f["path"] == path)
            .unwrap_or_else(|| panic!("{path} is not in the sample"));
        match entry["base64"].as_str() {
            Some(encoded) => decode_base64(encoded),
            None => entry["text"]
                .as_str()
                .expect("sample text")
                .as_bytes()
                .to_vec(),
        }
    }

    fn decode_base64(encoded: &str) -> Vec<u8> {
        const DIGITS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = Vec::new();
        let (mut acc, mut bits) = (0u32, 0u32);
        for byte in encoded
            .bytes()
            .filter(|b| *b != b'=' && !b.is_ascii_whitespace())
        {
            let digit = DIGITS
                .iter()
                .position(|d| *d == byte)
                .expect("base64 digit");
            acc = (acc << 6) | digit as u32;
            bits += 6;
            if bits >= 8 {
                bits -= 8;
                out.push((acc >> bits) as u8);
                acc &= (1 << bits) - 1;
            }
        }
        out
    }

    #[test]
    fn test_respond_with_each_format_and_map_matches_fresh_pack() {
        let (_, stats) = fresh(OutputFormat::Plain, true, false);
        assert_eq!((stats.files_extracted, stats.files_skipped), (3, 1));
        for source in [false, true] {
            for packed_with_map in [true, false] {
                let req = PackInput {
                    source,
                    ..input(project(), "xml", !packed_with_map)
                };
                let mut stored = extract(req).unwrap();
                for format in FORMATS {
                    for tree in [false, true] {
                        let got = respond(&mut stored, "w-1", format, tree).unwrap();
                        let (dump, stats) = fresh(format, tree, source);
                        let case = format!("{format:?} tree={tree} source={source}");
                        assert_eq!(got.dump, dump, "{case}");
                        assert_eq!(got.dump_bytes, dump.len(), "{case}");
                        assert_eq!(
                            (got.chars_emitted, got.tokens_est),
                            (stats.chars_emitted, stats.tokens_est),
                            "{case}"
                        );
                        assert_eq!(
                            (got.files_extracted, got.files_skipped),
                            (stats.files_extracted, stats.files_skipped),
                            "{case}"
                        );
                        assert_eq!(got.format, format.extension(), "{case}");
                        assert_eq!(got.filename, format!("pulp-browser.{}", format.extension()));
                        assert_eq!(got.result_id, "w-1");
                        assert_eq!(got.outcomes.len(), 4);
                        assert!(!got.preview_truncated);
                        assert!(got.error.is_none());
                    }
                }
            }
        }
    }

    #[test]
    fn test_respond_with_markdown_and_no_tree_drops_directory_map() {
        let mut stored = extract(input(project(), "xml", false)).unwrap();
        let with_map = respond(&mut stored, "w-1", OutputFormat::Markdown, true).unwrap();
        assert!(
            with_map.dump.starts_with("# Directory structure\n"),
            "{}",
            with_map.dump
        );
        let without = respond(&mut stored, "w-1", OutputFormat::Markdown, false).unwrap();
        assert!(
            without.dump.starts_with("## README.md\n"),
            "{}",
            without.dump
        );
        assert!(!without.dump.contains("Directory structure"));
        assert!(without.tokens_est < with_map.tokens_est);
        let again = respond(&mut stored, "w-1", OutputFormat::Markdown, true).unwrap();
        assert_eq!(again.dump, with_map.dump);
    }

    #[test]
    fn test_extract_keeps_pack_format_and_map_for_artifact() {
        let mut stored = extract(input(project(), "md", true)).unwrap();
        assert_eq!(stored.format, OutputFormat::Markdown);
        assert!(!stored.tree);
        let full = stored.render(stored.format, stored.tree).unwrap();
        assert_eq!(full, fresh(OutputFormat::Markdown, false, false).0);
    }

    #[test]
    fn test_respond_with_large_dump_caps_preview_and_render_keeps_full_dump() {
        let body = "x".repeat(DUMP_PREVIEW_BYTES + 1000) + "\n";
        let mut stored =
            extract(input(vec![file("big.txt", body.as_bytes())], "txt", false)).unwrap();
        let resp = respond(&mut stored, "w-1", OutputFormat::Plain, true).unwrap();
        assert!(resp.preview_truncated);
        assert!(resp.dump.ends_with("\n\n… preview truncated …\n"));
        assert!(resp.dump.len() < resp.dump_bytes);
        let full = stored.render(OutputFormat::Plain, true).unwrap();
        assert_eq!(full.len(), resp.dump_bytes);
        assert!(full.contains(&body));
    }

    #[test]
    fn test_respond_with_nothing_extracted_returns_error() {
        let mut stored =
            extract(input(vec![file("logo.png", &[0, 1, 2, 0])], "xml", false)).unwrap();
        let resp = respond(&mut stored, "w-1", OutputFormat::Xml, true).unwrap();
        assert_eq!(resp.files_extracted, 0);
        assert!(resp
            .error
            .as_deref()
            .is_some_and(|e| e.starts_with("Nothing extracted.")));
    }

    #[test]
    fn test_extract_with_hidden_on_keeps_hidden_files() {
        let files = || {
            vec![
                file("app/.github/ci.yml", b"on: push\n"),
                file("app/main.rs", b"fn main() {}\n"),
            ]
        };
        let off = extract(input(files(), "txt", true)).unwrap();
        assert_eq!(off.packed.stats.files_extracted, 1);
        let on = extract(PackInput {
            hidden: true,
            ..input(files(), "txt", true)
        })
        .unwrap();
        assert_eq!(on.packed.stats.files_extracted, 2);
    }

    #[test]
    fn test_result_store_with_fifth_result_evicts_least_recently_used() {
        let mut store = ResultStore::new();
        for id in ["a", "b", "c", "d"] {
            store.put(id.to_string(), stored_one());
        }
        assert!(store.get("a").is_some());
        store.put("e".to_string(), stored_one());
        assert!(store.get("b").is_none());
        for id in ["a", "c", "d", "e"] {
            assert!(store.get(id).is_some(), "{id}");
        }
        assert_eq!(store.entries.len(), MAX_RESULTS);
    }

    #[test]
    fn test_result_store_put_with_same_id_replaces_entry() {
        let mut store = ResultStore::new();
        store.put("a".to_string(), stored_one());
        store.put("a".to_string(), stored_one());
        assert_eq!(store.entries.len(), 1);
    }

    #[test]
    fn test_result_store_remove_with_missing_id_returns_false() {
        let mut store = ResultStore::new();
        store.put("a".to_string(), stored_one());
        assert!(store.remove("a"));
        assert!(!store.remove("a"));
        assert!(store.get("a").is_none());
    }

    #[test]
    fn test_preview_with_rust_source_returns_text_without_header() {
        let got = preview(preview_of("tides/src/lib.rs", b"pub fn tide() {}\n")).unwrap();
        assert_eq!(got.text, "pub fn tide() {}\n");
        assert_eq!(got.status, "extracted");
        assert!(got.message.is_empty());
        assert!(!got.truncated);
        assert_eq!(got.kind, "text");
    }

    #[test]
    fn test_preview_with_long_text_caps_on_char_boundary() {
        let body = "€".repeat(FILE_PREVIEW_BYTES);
        let got = preview(preview_of("notes.txt", body.as_bytes())).unwrap();
        assert!(got.truncated);
        assert_eq!(got.text.len(), FILE_PREVIEW_BYTES - FILE_PREVIEW_BYTES % 3);
        assert!(got.text.chars().all(|c| c == '€'));
    }

    #[test]
    fn test_preview_with_binary_returns_reason_without_text() {
        let got = preview(preview_of("assets/logo.png", &[0, 159, 146, 150, 0, 1])).unwrap();
        assert_eq!(got.status, "skipped_binary");
        assert!(got.text.is_empty());
        assert!(got.message.contains("binary"), "{}", got.message);
    }

    #[test]
    fn test_preview_with_hidden_file_follows_hidden_setting() {
        let off = preview(preview_of("app/.github/ci.yml", b"on: push\n")).unwrap();
        assert!(off.text.is_empty());
        assert!(!off.message.is_empty());
        let on = preview(PreviewInput {
            hidden: true,
            ..preview_of("app/.github/ci.yml", b"on: push\n")
        })
        .unwrap();
        assert_eq!(on.text, "on: push\n");
    }

    #[test]
    fn test_preview_with_sample_pdf_returns_field_notes() {
        let pdf = sample_file("docs/field-notes.pdf");
        let got = preview(preview_of("tides/docs/field-notes.pdf", &pdf)).unwrap();
        assert_eq!(got.status, "extracted");
        assert_eq!(got.kind, "pdf");
        assert!(got.text.contains("Field notes"), "{}", got.text);
    }

    #[test]
    fn test_preview_with_sample_archive_follows_archives_setting() {
        let zip = sample_file("data/archive.zip");
        let off = preview(preview_of("tides/data/archive.zip", &zip)).unwrap();
        assert_eq!(off.status, "skipped_archive");
        assert!(off.text.is_empty());
        assert!(!off.message.is_empty());
        let on = preview(PreviewInput {
            archives: true,
            ..preview_of("tides/data/archive.zip", &zip)
        })
        .unwrap();
        assert_eq!(on.status, "extracted");
        assert!(
            on.text.starts_with(
                "================================================\nFILE: data/archive.zip/"
            ),
            "{}",
            on.text
        );
        assert_eq!(on.kind, "zip");
    }

    #[test]
    fn test_extract_with_folder_grant_returns_folder_map_and_inner_paths() {
        let mut stored = extract(input(
            vec![
                file("tides/src/lib.rs", b"pub fn tide() {}\n"),
                file("tides/README.md", b"# Tides\n"),
            ],
            "txt",
            false,
        ))
        .unwrap();
        let got = respond(&mut stored, "w-1", OutputFormat::Plain, true).unwrap();
        let map = "Directory structure:\ntides/\n├── README.md\n└── src/\n    └── lib.rs\n";
        assert!(got.dump.starts_with(map), "{}", got.dump);
        assert!(got.dump.contains("FILE: src/lib.rs\n"), "{}", got.dump);
        assert!(!got.dump.contains("tides/src"), "{}", got.dump);
        let lib = got
            .outcomes
            .iter()
            .find(|o| o.relative == "src/lib.rs")
            .unwrap();
        assert_eq!(lib.id, "tides/src/lib.rs", "ids keep the scanned path");

        // The copy-tree key (`format_tree`) draws the same map.
        let ticked = [
            "tides/README.md".to_string(),
            "tides/src/lib.rs".to_string(),
        ];
        let (root, paths) = pulp::tree::split_grant_root(&ticked);
        let copied =
            pulp::render::format_directory_map(&root, &paths, OutputFormat::Plain).unwrap();
        assert_eq!(copied, map);
    }

    #[test]
    fn test_extract_with_loose_files_returns_dot_root() {
        let mut stored = extract(input(
            vec![file("a.rs", b"fn a() {}\n"), file("notes/b.md", b"# B\n")],
            "txt",
            false,
        ))
        .unwrap();
        let got = respond(&mut stored, "w-1", OutputFormat::Plain, true).unwrap();
        assert!(
            got.dump.starts_with("Directory structure:\n./\n"),
            "{}",
            got.dump
        );
        assert!(got.dump.contains("FILE: notes/b.md\n"), "{}", got.dump);
    }

    #[test]
    fn test_scan_and_extract_with_hidden_grant_folder_return_its_files() {
        let names = [
            ".dotfiles/zshrc",
            ".dotfiles/.secret",
            ".dotfiles/build/x.rs",
        ];
        let scanned = scan(ScanInput {
            files: names
                .iter()
                .map(|name| ScanFileIn {
                    relative: name.to_string(),
                    size: 4,
                    head: Vec::new(),
                })
                .collect(),
            hidden: false,
            archives: false,
            exclude: Vec::new(),
            max_file_size: None,
        })
        .unwrap();
        let listed: Vec<&str> = scanned.files.iter().map(|f| f.id.as_str()).collect();
        assert_eq!(
            listed,
            [".dotfiles/zshrc"],
            "hidden and build/ inside the folder stay out"
        );

        let stored = extract(input(
            names.iter().map(|name| file(name, b"text\n")).collect(),
            "txt",
            false,
        ))
        .unwrap();
        let packed: Vec<&str> = stored
            .packed
            .files
            .iter()
            .map(|f| f.relative.as_str())
            .collect();
        assert_eq!(packed, ["zshrc"]);
    }

    #[test]
    fn test_extract_with_corrupt_pdf_and_docx_returns_unreadable_outcomes() {
        let mut stored = extract(input(
            vec![
                file("tides/docs/paper.pdf", b"%PDF-1.4 garbage"),
                file("tides/docs/memo.docx", b"PK\x03\x04 not really a zip"),
                file("tides/src/lib.rs", b"pub fn tide() {}\n"),
            ],
            "txt",
            false,
        ))
        .unwrap();
        let got = respond(&mut stored, "w-1", OutputFormat::Plain, true).unwrap();
        assert_eq!((got.files_extracted, got.files_skipped), (1, 2));
        assert!(got.error.is_none(), "{:?}", got.error);
        for (relative, kind) in [("docs/paper.pdf", "pdf"), ("docs/memo.docx", "docx")] {
            let outcome = got
                .outcomes
                .iter()
                .find(|o| o.relative == relative)
                .unwrap_or_else(|| panic!("no outcome for {relative}"));
            assert_eq!(outcome.status, "unreadable", "{relative}");
            let file = stored
                .packed
                .files
                .iter()
                .find(|f| f.relative == relative)
                .unwrap();
            let FileStatus::Unreadable(reason) = &file.status else {
                panic!("{relative}: {:?}", file.status);
            };
            assert!(
                outcome.message.ends_with(reason.as_str()),
                "{}",
                outcome.message
            );
            let note = format!("[{kind} unreadable: {reason}]");
            assert!(got.dump.contains(&note), "{}", got.dump);
        }
    }

    #[test]
    fn test_respond_with_only_a_corrupt_pdf_returns_nothing_extracted_error() {
        let mut stored = extract(input(
            vec![file("paper.pdf", b"%PDF-1.4 garbage")],
            "txt",
            false,
        ))
        .unwrap();
        let got = respond(&mut stored, "w-1", OutputFormat::Plain, true).unwrap();
        assert_eq!(got.files_extracted, 0);
        let error = got.error.unwrap_or_default();
        assert!(error.contains("paper.pdf [unreadable]: "), "{error}");
        assert!(got.dump.contains("[pdf unreadable: "), "{}", got.dump);
    }

    #[test]
    fn test_preview_with_corrupt_pdf_returns_unreadable_reason() {
        let got = preview(preview_of("tides/docs/paper.pdf", b"%PDF-1.4 garbage")).unwrap();
        assert_eq!(got.status, "unreadable");
        assert_eq!(got.kind, "pdf");
        assert!(got.text.is_empty(), "{}", got.text);
        assert!(got.message.contains("one-line note"), "{}", got.message);
    }
}
