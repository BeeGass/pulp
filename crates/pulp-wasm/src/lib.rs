//! Browser bindings over pulp's extract/pack core (no filesystem).
//!
//! File bytes cross the JS↔WASM boundary as `Uint8Array` (not base64).
//! Each instance keeps the extracted files of its last few packs, so the mill
//! can redraw a dump in another format without extracting again.
//!
//! A pack runs in one call ([`pack_files`]) or in steps, the way the mill's
//! worker runs it: [`pack_begin`] plans the pack, [`extract_files`] turns
//! batches of files into text in another instance, [`pack_add`] collects each
//! batch, and [`pack_finish`] stores the result. A parser that panics, traps,
//! or overflows its stack takes down only the instance extracting its batch,
//! so the worker can note that file in the dump and carry on, as `pulp ui`
//! does with its child processes.
//!
//! A stepped pack may name the result it replaces. Files whose name, size,
//! and modification time match what that result was made from, under the
//! same extraction settings, are taken from it instead of being read and
//! extracted again.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};

use pulp::{
    apply_budgets, classify, cmp_path_order, is_default_selected, kind_from_label, language_name,
    pack_entries, FileStatus, Kind, MemoryFile, Options, OutputFormat, Packed, PackedFile,
    PathPolicy, Selection, Stats, TreeMode,
};
use serde::ser::Serialize as SerdeSerialize;
use serde::{Deserialize, Serialize};
use wasm_bindgen::prelude::*;

const DEFAULT_MAX_FILE_SIZE: u64 = 8 * 1024 * 1024;
/// Longest dump returned inline, the same as `pulp ui` shows. Copy and
/// download fetch the rest.
const DUMP_PREVIEW_BYTES: usize = 32 * 1024;
/// Longest text returned by [`preview_file`].
const FILE_PREVIEW_BYTES: usize = 32 * 1024;
/// Pack results each instance keeps for redraws and full dumps.
const MAX_RESULTS: usize = 4;
const RESULT_GONE: &str = "the dump is no longer in memory; pulp again";
/// Name of the JS error a panic throws. The instance that threw it must not be
/// used again.
const PANIC_NAME: &str = "PulpPanic";
/// Why a file the tab could no longer read is left out. A browser keeps a
/// snapshot of each chosen file and refuses to read it once the file changes,
/// moves, or loses its read permission.
const CHANGED_MESSAGE: &str = "This file changed, moved, or lost its read permission after it was chosen, so the tab can no longer read it. Choose the folder again, then pulp again so the dump holds its current bytes.";
/// Dump note for such a file, as `pulp ui` writes one. A failure's message,
/// such as `no longer there` for a file that is gone, follows a colon.
const CHANGED_NOTE: &str = "changed since scan";
/// Seconds a parser `pulp ui` would run in a child process gets before the
/// mill stops it, as `pulp ui` stops the child.
const EXTRACT_TIMEOUT_SECS: u32 = 30;
/// Why a file the page could not hand to a worker was not extracted.
const NO_WORKER_MESSAGE: &str =
    "extractor not run: the page could not start a worker to parse this file in";

#[wasm_bindgen(start)]
pub fn start() {
    std::panic::set_hook(Box::new(|info| {
        console_error_panic_hook::hook(info);
        // panic=abort turns the trap into the JS message "unreachable".
        // Throwing here keeps the panic text on the error the mill shows, and
        // the name tells the page this instance is spent.
        let err = js_sys::Error::new(&info.to_string());
        err.set_name(PANIC_NAME);
        let payload = panic_payload(info.payload());
        let _ = js_sys::Reflect::set(&err, &JsValue::from_str("payload"), &payload.into());
        wasm_bindgen::throw_val(err.into());
    }));
}

/// The message a panic was raised with, as `pulp` reports a parser panic.
fn panic_payload(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        return (*message).to_string();
    }
    if let Some(message) = payload.downcast_ref::<String>() {
        return message.clone();
    }
    "unknown panic".to_string()
}

#[wasm_bindgen]
pub fn pulp_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

/// Whether files of `kind` (a label such as `pdf` or `docx`, as a scan
/// reports it) have the parsers `pulp ui` runs in a child process with a time
/// limit. The mill runs them on their own, under [`extract_timeout_ms`].
#[wasm_bindgen]
pub fn heavy_kind(kind: &str) -> bool {
    kind_from_label(kind).is_some_and(pulp::extract::isolate::needs_isolation)
}

/// How long the mill gives one [`heavy_kind`] file before it stops its
/// extractor, in milliseconds: `pulp ui`'s limit for a child process.
#[wasm_bindgen]
pub fn extract_timeout_ms() -> u32 {
    EXTRACT_TIMEOUT_SECS * 1000
}

/// Whether extracting a file of `kind` can run a parser [`heavy_kind`]
/// covers: the file is of a heavy kind, or it is an archive a pack with
/// `archives` on expands, whose members may be. The page never runs such a
/// parser on its own thread, where nothing could stop one that hangs.
#[wasm_bindgen]
pub fn needs_worker(kind: &str, archives: bool) -> bool {
    kind_from_label(kind).is_some_and(|kind| {
        pulp::extract::isolate::needs_isolation(kind) || (archives && kind.is_archive())
    })
}

fn to_js<T: serde::Serialize>(value: &T) -> Result<JsValue, JsValue> {
    let serializer = serde_wasm_bindgen::Serializer::new().serialize_maps_as_objects(true);
    SerdeSerialize::serialize(value, &serializer).map_err(|e| JsValue::from_str(&e.to_string()))
}

fn from_js<T: serde::de::DeserializeOwned>(input: JsValue, what: &str) -> Result<T, JsValue> {
    serde_wasm_bindgen::from_value(input)
        .map_err(|e| JsValue::from_str(&format!("invalid {what} input: {e}")))
}

fn js_now_ms() -> f64 {
    js_sys::Date::now()
}

/// Milliseconds since `t0`, as a JS number.
fn elapsed_since(t0: f64) -> u64 {
    (js_now_ms() - t0).max(0.0) as u64
}

/// File bytes from a `Uint8Array` in one copy. Serde reads a plain `Vec<u8>`
/// field as a JS iterable, one element at a time, which costs about a second
/// for an 8 MiB file.
mod bytes {
    use serde::de::{Deserializer, SeqAccess, Visitor};

    pub fn deserialize<'de, D: Deserializer<'de>>(de: D) -> Result<Vec<u8>, D::Error> {
        de.deserialize_byte_buf(Bytes)
    }

    struct Bytes;

    impl<'de> Visitor<'de> for Bytes {
        type Value = Vec<u8>;

        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("bytes")
        }

        fn visit_byte_buf<E>(self, v: Vec<u8>) -> Result<Vec<u8>, E> {
            Ok(v)
        }

        fn visit_bytes<E>(self, v: &[u8]) -> Result<Vec<u8>, E> {
            Ok(v.to_vec())
        }

        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Vec<u8>, A::Error> {
            let mut out = Vec::with_capacity(seq.size_hint().unwrap_or(0));
            while let Some(byte) = seq.next_element()? {
                out.push(byte);
            }
            Ok(out)
        }
    }
}

/// A granted path without a leading `./` or `/`. Browsers always separate a
/// granted path with `/`, so a `\` is part of a file name and stays, as the
/// local mill keeps it.
fn normalize_rel(path: &str) -> String {
    path.trim_start_matches("./")
        .trim_start_matches('/')
        .to_string()
}

/// The path filters of a scan or pack with `opts`: the built-in excludes when
/// they are on, then the page's own.
fn policy_for(opts: &Options) -> Result<PathPolicy, String> {
    PathPolicy::from_options(opts).map_err(|e| e.to_string())
}

/// The folder a folder pick named every path after (`tides` for
/// `tides/src/lib.rs`), or `.` when the paths share no folder. The same
/// answer as [`pulp::tree::split_grant_root`] without splitting every path.
fn grant_root<'a>(relatives: impl IntoIterator<Item = &'a str>) -> String {
    let mut label: Option<&str> = None;
    for relative in relatives {
        let mut parts = relative
            .split('/')
            .filter(|part| !part.is_empty() && *part != "." && *part != "..");
        let Some(first) = parts.next() else {
            continue;
        };
        if parts.next().is_none() || label.is_some_and(|label| label != first) {
            return ".".to_string();
        }
        label = Some(first);
    }
    label.map_or_else(|| ".".to_string(), str::to_string)
}

/// The folder a scan or pack looks from inside of: `named`, the grant folder
/// a scan reported, when the page passes one; else the folder `relatives`
/// share.
fn root_or_shared<'a>(named: Option<&str>, relatives: impl IntoIterator<Item = &'a str>) -> String {
    match named.map(str::trim) {
        Some(root) if !root.is_empty() => root.to_string(),
        _ => grant_root(relatives),
    }
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

/// Keeps the first `max` bytes written to it and counts the rest.
struct Head {
    buf: Vec<u8>,
    max: usize,
    total: usize,
}

impl std::io::Write for Head {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        let room = self.max.saturating_sub(self.buf.len());
        self.buf.extend_from_slice(&data[..data.len().min(room)]);
        self.total = self.total.saturating_add(data.len());
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Pack settings. They sit in the same object as the files they apply to.
#[derive(Clone, Default, Deserialize)]
struct Settings {
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
    /// Globs excluded on top of the built-in list.
    #[serde(default)]
    exclude: Vec<String>,
    #[serde(default)]
    no_default_excludes: bool,
    #[serde(default)]
    max_file_size: Option<u64>,
    #[serde(default)]
    max_total_bytes: Option<u64>,
    #[serde(default)]
    max_entries: Option<usize>,
    /// The grant folder, as [`scan_files`] reported it. Missing, it is the
    /// folder the packed paths share.
    #[serde(default)]
    root: Option<String>,
}

impl Settings {
    /// The settings a file's extracted text and outcome depend on: those for
    /// every file, and those an archive's members also depend on. Every other
    /// setting only draws the dump (`format`, `no_tree`), picks the files
    /// (the budgets), or names them (`root`, which each file's name already
    /// carries), so a pack that changes it can still reuse a file.
    fn extraction(&self) -> (TextSettings, MemberSettings) {
        // Listed whole, so a new setting cannot be left out unnoticed.
        let Self {
            format: _,
            archives,
            binaries,
            notebook_outputs,
            source,
            hidden,
            no_tree: _,
            exclude,
            no_default_excludes,
            max_file_size,
            max_total_bytes: _,
            max_entries: _,
            root: _,
        } = self;
        (
            TextSettings {
                binaries: *binaries,
                notebook_outputs: *notebook_outputs,
                source: *source,
                max_file_size: max_file_size.unwrap_or(DEFAULT_MAX_FILE_SIZE),
            },
            MemberSettings {
                archives: *archives,
                hidden: *hidden,
                exclude: exclude.clone(),
                no_default_excludes: *no_default_excludes,
            },
        )
    }

    /// Options for a pack from inside the grant folder `root`. The directory
    /// map is always built, so a redraw can show it; rendering leaves it out
    /// when it is off.
    fn options(&self, root: &str) -> Options {
        let defaults = Options::default();
        Options {
            roots: vec![PathBuf::from(root)],
            format: parse_format(&self.format),
            follow_archives: self.archives,
            skip_binaries: !self.binaries,
            notebook_outputs: self.notebook_outputs,
            source_mode: self.source,
            hidden: self.hidden,
            tree: TreeMode::Selected,
            exclude: self.exclude.clone(),
            default_excludes: !self.no_default_excludes,
            max_file_size: self.max_file_size.unwrap_or(DEFAULT_MAX_FILE_SIZE),
            max_total_bytes: self.max_total_bytes.unwrap_or(defaults.max_total_bytes),
            max_entries: self.max_entries.unwrap_or(0),
            selection: Selection::AllEligible,
            jobs: 1,
            ..defaults
        }
    }
}

thread_local! {
    static RESULTS: RefCell<ResultStore> = const { RefCell::new(ResultStore::new()) };
    static PENDING: RefCell<Option<Pending>> = const { RefCell::new(None) };
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
    /// What a later pack needs to take files from this result. Only a stepped
    /// pack records it.
    reuse: Option<Reusable>,
}

/// What a file's text and outcome depend on besides its bytes and its name.
#[derive(Clone, Debug, PartialEq)]
struct TextSettings {
    binaries: bool,
    notebook_outputs: bool,
    source: bool,
    max_file_size: u64,
}

/// What an archive's entries also depend on: whether it is unpacked, and the
/// filters its members pass.
#[derive(Clone, Debug, PartialEq)]
struct MemberSettings {
    archives: bool,
    hidden: bool,
    exclude: Vec<String>,
    no_default_excludes: bool,
}

/// How one ticked file was extracted, so a later pack of the same bytes under
/// the same settings can take its entries instead of extracting it again.
#[derive(Clone, Debug)]
struct Extraction {
    /// The name inside the root it was packed under.
    relative: String,
    size: u64,
    /// The modification time the page read for it, in milliseconds.
    modified: f64,
    /// Positions in the result's files of the entries it became: its own, or
    /// an archive's.
    outputs: Vec<usize>,
    /// It is an archive, so its entries also depend on [`MemberSettings`].
    archive: bool,
    /// Its archive stopped at the archive budget, which marks a pack truncated.
    cut: bool,
}

/// The settings a result was extracted under, and its files by the id of the
/// ticked file each came from.
#[derive(Debug)]
struct Reusable {
    text: TextSettings,
    members: MemberSettings,
    inputs: HashMap<String, Extraction>,
}

impl Reusable {
    /// What the result holds for a file named `relative`, `size` bytes long,
    /// last modified at `modified`, to be packed under `text` and `members`.
    fn find(
        &self,
        id: &str,
        relative: &str,
        size: u64,
        modified: Option<f64>,
        text: &TextSettings,
        members: &MemberSettings,
    ) -> Option<&Extraction> {
        let modified = modified?;
        if self.text != *text {
            return None;
        }
        self.inputs.get(id).filter(|e| {
            e.relative == relative
                && e.size == size
                && e.modified == modified
                && (!e.archive || self.members == *members)
        })
    }
}

impl StoredResult {
    /// The whole dump in `format`, with or without the directory map.
    fn render(&mut self, format: OutputFormat, tree: bool) -> Result<String, String> {
        let mut buf = Vec::new();
        self.write(&mut buf, format, tree)?;
        String::from_utf8(buf).map_err(|e| format!("render failed: {e}"))
    }

    /// The first `max` bytes of the dump, cut on a character boundary, and
    /// the whole dump's length. The rest is counted, never held.
    fn render_head(
        &mut self,
        format: OutputFormat,
        tree: bool,
        max: usize,
    ) -> Result<(String, usize), String> {
        let mut head = Head {
            buf: Vec::new(),
            max,
            total: 0,
        };
        self.write(&mut head, format, tree)?;
        let mut buf = head.buf;
        if let Err(err) = std::str::from_utf8(&buf) {
            buf.truncate(err.valid_up_to());
        }
        let text = String::from_utf8(buf).map_err(|e| format!("render failed: {e}"))?;
        Ok((text, head.total))
    }

    fn write(
        &mut self,
        out: &mut impl std::io::Write,
        format: OutputFormat,
        tree: bool,
    ) -> Result<(), String> {
        let opts = Options {
            format,
            source_mode: self.source_mode,
            ..Options::default()
        };
        // The writers print whatever map `packed.tree` holds, so lift it out while it is off.
        let map = (!tree).then(|| std::mem::take(&mut self.packed.tree));
        let written = pulp::render::write_all(out, &self.packed, &opts);
        if let Some(map) = map {
            self.packed.tree = map;
        }
        written.map_err(|e| format!("render failed: {e}"))
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

    /// The result stored under `id`, leaving the order alone.
    fn peek(&self, id: &str) -> Option<&StoredResult> {
        self.entries
            .iter()
            .find(|(key, _)| key == id)
            .map(|(_, result)| result)
    }

    /// Take the result stored under `id` out of the store.
    fn take(&mut self, id: &str) -> Option<StoredResult> {
        let at = self.entries.iter().position(|(key, _)| key == id)?;
        self.entries.remove(at).map(|(_, result)| result)
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

/// Keep `stored` and draw it as the pack asked. `elapsed_ms` is left at zero.
fn store_and_respond(stored: StoredResult) -> Result<PackResponse, String> {
    let result_id = new_handle();
    RESULTS.with(|results| {
        let mut results = results.borrow_mut();
        let stored = results.put(result_id.clone(), stored);
        let (format, tree) = (stored.format, stored.tree);
        respond(stored, &result_id, format, tree)
    })
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
    /// The folder every granted path starts with, or `.` for loose files and
    /// several folders. Scanned paths start inside it, as `pulp ui` lists a
    /// folder; ids keep the granted path.
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
    elapsed_ms: u64,
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

#[derive(Deserialize)]
struct ScanFileIn {
    relative: String,
    #[serde(default)]
    size: u64,
    /// Optional leading bytes so unknown extensions can be sniffed.
    #[serde(default, deserialize_with = "bytes::deserialize")]
    head: Vec<u8>,
}

#[derive(Deserialize)]
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
    no_default_excludes: bool,
    #[serde(default)]
    max_file_size: Option<u64>,
    #[serde(default)]
    max_total_bytes: Option<u64>,
    /// The grant folder, as [`scan_keep`] found it. Missing, it is the folder
    /// the listed paths share.
    #[serde(default)]
    root: Option<String>,
}

impl ScanInput {
    fn options(&self) -> Options {
        let defaults = Options::default();
        Options {
            hidden: self.hidden,
            follow_archives: self.archives,
            exclude: self.exclude.clone(),
            default_excludes: !self.no_default_excludes,
            max_file_size: self.max_file_size.unwrap_or(DEFAULT_MAX_FILE_SIZE),
            max_total_bytes: self.max_total_bytes.unwrap_or(defaults.max_total_bytes),
            ..defaults
        }
    }

    fn policy(&self) -> Result<PathPolicy, String> {
        policy_for(&self.options())
    }
}

/// Classify a file list without reading contents (extension / name based).
/// A file with a `head` is also classified by its leading bytes.
#[wasm_bindgen]
pub fn scan_files(input: JsValue) -> Result<JsValue, JsValue> {
    let req: ScanInput = from_js(input, "scan")?;
    let resp = scan(req).map_err(|e| JsValue::from_str(&e))?;
    to_js(&resp)
}

/// Indices of the files in a scan input that [`scan_files`] would list but
/// cannot classify by name. Read their leading bytes into `head` and scan again.
#[wasm_bindgen]
pub fn scan_unknown(input: JsValue) -> Result<Vec<u32>, JsValue> {
    let req: ScanInput = from_js(input, "scan")?;
    unknown(&req).map_err(|e| JsValue::from_str(&e))
}

fn unknown(req: &ScanInput) -> Result<Vec<u32>, String> {
    let policy = req.policy()?;
    let relatives: Vec<String> = req
        .files
        .iter()
        .map(|f| normalize_rel(&f.relative))
        .collect();
    let root = root_or_shared(req.root.as_deref(), relatives.iter().map(String::as_str));
    Ok(relatives
        .iter()
        .enumerate()
        .filter(|(_, relative)| {
            !relative.is_empty()
                && policy.keep_walk(under_root(relative, &root))
                && pulp::classify::kind_from_name(Path::new(relative.as_str())).is_none()
        })
        .map(|(i, _)| i as u32)
        .collect())
}

/// A grant's paths, and the scan settings that filter them.
#[derive(Default, Deserialize)]
struct KeepInput {
    #[serde(default)]
    relatives: Vec<String>,
    #[serde(default)]
    hidden: bool,
    #[serde(default)]
    archives: bool,
    #[serde(default)]
    exclude: Vec<String>,
    #[serde(default)]
    no_default_excludes: bool,
}

#[derive(Serialize)]
struct KeepResponse {
    /// The grant folder, as [`ScanResponse::root`] reports it.
    root: String,
    /// Positions of the paths the filters keep.
    keep: Vec<u32>,
}

/// Which of a grant's paths the scan's filters keep: `{ relatives, hidden,
/// archives }` to `{ root, keep }`. Reads no file, so a page can learn which
/// picked files it needs the size of before it asks the browser for any.
#[wasm_bindgen]
pub fn scan_keep(input: JsValue) -> Result<JsValue, JsValue> {
    let req: KeepInput = from_js(input, "scan")?;
    let resp = keep(req).map_err(|e| JsValue::from_str(&e))?;
    to_js(&resp)
}

/// A file name no filter names, to ask whether the filters would keep a file
/// inside a folder.
const FOLDER_PROBE: &str = "\u{1}pulp-folder-probe\u{1}";

/// A scan's filters, compiled once, for a page that walks a granted folder
/// itself: which folders it can leave unopened because the filters leave out
/// every file inside them (`target/`, `node_modules/`, hidden folders while
/// hidden files are off). Skipping them changes no scan, only its time.
#[wasm_bindgen]
pub struct FolderFilter {
    policy: PathPolicy,
}

#[wasm_bindgen]
impl FolderFilter {
    /// The filters of a scan with `{ hidden, archives, exclude?,
    /// no_default_excludes? }`, the settings [`scan_keep`] takes.
    #[wasm_bindgen(constructor)]
    pub fn new(input: JsValue) -> Result<FolderFilter, JsValue> {
        let req: KeepInput = from_js(input, "folder filter")?;
        Self::from_keep(&req).map_err(|e| JsValue::from_str(&e))
    }

    /// Whether a walk should open the folder at `relative`, a path inside
    /// the grant folder.
    pub fn enter(&self, relative: &str) -> bool {
        let relative = normalize_rel(relative);
        let relative = relative.trim_end_matches('/');
        relative.is_empty() || self.policy.keep_walk(&format!("{relative}/{FOLDER_PROBE}"))
    }
}

impl FolderFilter {
    fn from_keep(req: &KeepInput) -> Result<Self, String> {
        Ok(Self {
            policy: req.scan_input().policy()?,
        })
    }
}

impl KeepInput {
    /// The scan these settings describe, with no files.
    fn scan_input(&self) -> ScanInput {
        ScanInput {
            files: Vec::new(),
            hidden: self.hidden,
            archives: self.archives,
            exclude: self.exclude.clone(),
            no_default_excludes: self.no_default_excludes,
            max_file_size: None,
            max_total_bytes: None,
            root: None,
        }
    }
}

fn keep(req: KeepInput) -> Result<KeepResponse, String> {
    let scan = req.scan_input();
    let policy = scan.policy()?;
    let relatives: Vec<String> = req.relatives.iter().map(|r| normalize_rel(r)).collect();
    let root = root_or_shared(scan.root.as_deref(), relatives.iter().map(String::as_str));
    let keep = relatives
        .iter()
        .enumerate()
        .filter(|(_, relative)| {
            !relative.is_empty() && policy.keep_walk(under_root(relative, &root))
        })
        .map(|(i, _)| i as u32)
        .collect();
    Ok(KeepResponse { root, keep })
}

/// The files a grant offers for packing, filtered as the local mill filters a
/// walk from inside the granted folder. The entry and byte budgets are spent
/// in path order as the walk spends them: from the first file that does not
/// fit, the rest are left out and the scan is marked truncated. A file over
/// the per-file cap is listed, spending none of the byte budget.
fn scan(req: ScanInput) -> Result<ScanResponse, String> {
    let opts = req.options();
    let policy = policy_for(&opts)?;
    let archives = req.archives;
    let relatives: Vec<String> = req
        .files
        .iter()
        .map(|f| normalize_rel(&f.relative))
        .collect();
    let root = root_or_shared(req.root.as_deref(), relatives.iter().map(String::as_str));

    // (file, granted path, path inside the root)
    let mut kept: Vec<(ScanFileIn, String, String)> = req
        .files
        .into_iter()
        .zip(relatives)
        .filter_map(|(f, relative)| {
            let inner = under_root(&relative, &root).to_string();
            (!relative.is_empty() && policy.keep_walk(&inner)).then_some((f, relative, inner))
        })
        .collect();
    // The budgets are spent in path order, as the local mill's walk spends them.
    kept.sort_by(|a, b| cmp_path_order(&a.2, &b.2).then_with(|| a.1.cmp(&b.1)));
    let truncated = apply_budgets(&mut kept, &opts, |f| f.0.size);

    let mut files = Vec::with_capacity(kept.len());
    let mut bytes = 0u64;
    for (f, relative, inner) in kept {
        let oversized = f.size > opts.max_file_size;
        let sniff = (!f.head.is_empty()).then_some(f.head.as_slice());
        let path = Path::new(&inner);
        let kind = classify(path, sniff);
        // Generated and virtualenv folders count only inside the grant, as the
        // local mill judges them under the folder it scans: a folder picked
        // from `~/build/app`, or named `venv`, still starts ticked.
        let default_on =
            is_default_selected(path, kind) && !oversized && (!kind.is_archive() || archives);
        bytes = bytes.saturating_add(f.size);
        files.push(FileEntry {
            language: language_name(path, kind),
            // The id keeps the granted path, which names the file in the tab.
            id: relative,
            relative: inner,
            size: f.size,
            kind: kind.as_str(),
            default_on,
            oversized,
        });
    }

    let file_count = files.len();
    Ok(ScanResponse {
        root,
        files,
        file_count,
        bytes,
        truncated,
        manifest_id: new_handle(),
    })
}

#[derive(Deserialize)]
struct PackFileIn {
    relative: String,
    #[serde(default)]
    id: String,
    #[serde(default, deserialize_with = "bytes::deserialize")]
    bytes: Vec<u8>,
}

/// The files of a one-call pack. The settings sit in the same object.
#[derive(Default, Deserialize)]
struct PackInput {
    #[serde(default)]
    files: Vec<PackFileIn>,
    #[serde(default)]
    selected: Vec<String>,
}

#[derive(Deserialize)]
struct RenderInput {
    result_id: String,
    #[serde(default)]
    format: String,
    #[serde(default)]
    no_tree: bool,
}

/// Why a file has no text: `changed` for a file the browser no longer reads,
/// `timeout` for a parser stopped at [`extract_timeout_ms`], `no_worker` for a
/// heavy file a page without workers leaves alone, else an error whose
/// `message` says what went wrong.
#[derive(Clone, Default, Deserialize)]
struct Failure {
    #[serde(default)]
    reason: String,
    #[serde(default)]
    message: String,
}

/// One file for [`preview_file`], with the pack settings that decide its text.
#[derive(Default, Deserialize)]
struct PreviewInput {
    relative: String,
    #[serde(default, deserialize_with = "bytes::deserialize")]
    bytes: Vec<u8>,
    /// Size of the file on disk, for the message when it cannot be read.
    #[serde(default)]
    size: Option<u64>,
    /// The kind the scan found, for the note when the file has no text.
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    failure: Option<Failure>,
}

#[derive(Deserialize)]
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
    let req: TreeInput = from_js(input, "tree")?;
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
fn extract(req: PackInput, settings: &Settings) -> Result<StoredResult, String> {
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
    let root = root_or_shared(
        settings.root.as_deref(),
        files.iter().map(|f| f.relative.as_str()),
    );
    let opts = Options {
        selection: Selection::Only(req.selected.iter().map(|s| normalize_rel(s)).collect()),
        ..settings.options(&root)
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
        format: opts.format,
        tree: !settings.no_tree,
        source_mode: settings.source,
        extract_ms: 0.0,
        reuse: None,
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
    // Only the part on screen is drawn in memory; copy and download draw the rest.
    let (dump, dump_bytes) = stored.render_head(format, tree, DUMP_PREVIEW_BYTES)?;
    let preview_truncated = dump_bytes > dump.len();
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

/// The mill's message for a file's status.
fn status_message(status: &FileStatus, size: u64) -> String {
    match status {
        FileStatus::Changed => CHANGED_MESSAGE.to_string(),
        other => other.message(size),
    }
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
            message: status_message(&f.status, f.size),
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
    let settings: Settings = from_js(input.clone(), "pack")?;
    let req: PackInput = from_js(input, "pack")?;
    let mut stored = extract(req, &settings).map_err(|e| JsValue::from_str(&e))?;
    stored.extract_ms = js_now_ms() - t0;
    let mut resp = store_and_respond(stored).map_err(|e| JsValue::from_str(&e))?;
    resp.elapsed_ms = elapsed_since(t0);
    to_js(&resp)
}

/// One file of a stepped pack, as the page names it.
#[derive(Deserialize)]
struct PlanFileIn {
    relative: String,
    #[serde(default)]
    id: String,
    #[serde(default)]
    size: u64,
    /// The kind the scan found, by name or by leading bytes (a label such as
    /// `pdf`). Missing, the file is classified by name.
    #[serde(default)]
    kind: Option<String>,
    /// When the file was last modified, in milliseconds, as the page read it
    /// for this pack. Missing, the file is never taken from an earlier result.
    #[serde(default)]
    modified: Option<f64>,
    /// The size the scan listed the file at, when the page reads it afresh
    /// for the pack. The budgets are spent by it, as `pulp ui` spends them at
    /// its scan and then reads each file as it is now.
    #[serde(default)]
    scanned: Option<u64>,
}

#[derive(Deserialize)]
struct PlanInput {
    #[serde(default)]
    files: Vec<PlanFileIn>,
    /// The scan left files out at its budgets, so the pack is cut short too,
    /// as `pulp ui` reports a pack of a cut scan.
    #[serde(default)]
    truncated: bool,
    /// The result this pack replaces. Files that did not change since it was
    /// made are taken from it, and it is dropped when the pack finishes.
    #[serde(default)]
    replaces: Option<String>,
}

/// A file [`pack_begin`] kept, in the order the pack extracts it.
#[derive(Clone, Serialize)]
struct PlannedFile {
    /// Position of the file in the input.
    index: u32,
    id: String,
    /// Path inside the grant folder, the name the dump gives the file.
    relative: String,
    /// The kind the pack goes by, as [`heavy_kind`] reads it.
    kind: &'static str,
    /// Its kind has a parser `pulp ui` runs in a child process with a time
    /// limit ([`heavy_kind`]); the kind is the scan's, so a PDF with no
    /// extension counts.
    heavy: bool,
    /// Extract on its own: a heavy file or an archive.
    alone: bool,
    /// Taken from the result the pack replaces: neither read nor extracted.
    cached: bool,
}

#[derive(Serialize)]
struct PlanResponse {
    root: String,
    files: Vec<PlannedFile>,
    truncated: bool,
}

/// A stepped pack between [`pack_begin`] and [`pack_finish`].
struct Pending {
    opts: Options,
    no_tree: bool,
    files: Vec<PackedFile>,
    truncated: bool,
    bytes_read: u64,
    started_ms: f64,
    /// Each planned file's name, size, and modification time, by id.
    planned: HashMap<String, Fingerprint>,
    /// The files extracted so far and the entries each became.
    done: Vec<InputDone>,
    /// The result this pack replaces, and what it takes from it.
    replaces: Option<String>,
    reused: Vec<(String, Extraction)>,
    text: TextSettings,
    members: MemberSettings,
}

/// The name, size, and modification time a file was planned with.
struct Fingerprint {
    relative: String,
    size: u64,
    modified: Option<f64>,
}

/// A file of a stepped pack as [`plan`] weighs it.
struct Candidate {
    index: u32,
    id: String,
    relative: String,
    size: u64,
    /// What it spends of the budgets.
    budget: u64,
    kind: Kind,
    modified: Option<f64>,
}

/// The kind a scan reported for a file, else the kind its name says.
fn known_kind(label: Option<&str>, relative: &str) -> Kind {
    label
        .and_then(kind_from_label)
        .unwrap_or_else(|| classify(Path::new(relative), None))
}

/// Which files a stepped pack extracts, and under what names. Mirrors the
/// checks [`pack_entries`] makes before it reads a file: the path filters, the
/// entry cap, and the byte budget, taken in path order. A file that
/// `previous`, the result the pack replaces, holds as it is now, under the
/// same settings, is marked `cached`: the pack takes its entries from there.
fn plan(
    req: PlanInput,
    settings: &Settings,
    previous: Option<&Reusable>,
) -> Result<(PlanResponse, Pending), String> {
    let scan_cut = req.truncated;
    let replaces = req.replaces.filter(|id| !id.is_empty());
    let files: Vec<Candidate> = req
        .files
        .into_iter()
        .enumerate()
        .filter_map(|(i, f)| {
            let relative = normalize_rel(&f.relative);
            if relative.is_empty() {
                return None;
            }
            let id = if f.id.is_empty() {
                relative.clone()
            } else {
                f.id
            };
            Some(Candidate {
                index: i as u32,
                id,
                kind: known_kind(f.kind.as_deref(), &relative),
                relative,
                size: f.size,
                budget: f.scanned.unwrap_or(f.size),
                modified: f.modified,
            })
        })
        .collect();
    let root = root_or_shared(
        settings.root.as_deref(),
        files.iter().map(|f| f.relative.as_str()),
    );
    let opts = settings.options(&root);
    let policy = policy_for(&opts)?;
    let mut kept: Vec<Candidate> = files
        .into_iter()
        .filter_map(|mut c| {
            let inner = under_root(&c.relative, &root);
            if !policy.keep_walk(inner) {
                return None;
            }
            c.relative = inner.to_string();
            Some(c)
        })
        .collect();
    // The budgets are spent in path order, as `pack_entries` and the walk spend them.
    kept.sort_by(|a, b| cmp_path_order(&a.relative, &b.relative).then_with(|| a.id.cmp(&b.id)));
    let truncated = apply_budgets(&mut kept, &opts, |c| c.budget);

    let (text, members) = settings.extraction();
    let mut planned = Vec::with_capacity(kept.len());
    let mut fingerprints = HashMap::with_capacity(kept.len());
    let mut reused = Vec::new();
    let mut cut = false;
    let mut bytes_read = 0u64;
    for c in kept {
        if c.size <= opts.max_file_size {
            bytes_read = bytes_read.saturating_add(c.size);
        }
        let found = previous
            .and_then(|prev| prev.find(&c.id, &c.relative, c.size, c.modified, &text, &members));
        let cached = found.is_some();
        match found {
            Some(extraction) => {
                cut |= extraction.cut;
                reused.push((c.id.clone(), extraction.clone()));
            }
            None => {
                fingerprints.insert(
                    c.id.clone(),
                    Fingerprint {
                        relative: c.relative.clone(),
                        size: c.size,
                        modified: c.modified,
                    },
                );
            }
        }
        let heavy = pulp::extract::isolate::needs_isolation(c.kind);
        planned.push(PlannedFile {
            index: c.index,
            id: c.id,
            relative: c.relative,
            kind: c.kind.as_str(),
            heavy,
            alone: heavy || c.kind.is_archive(),
            cached,
        });
    }
    let pending = Pending {
        opts,
        no_tree: settings.no_tree,
        files: Vec::new(),
        truncated: truncated || scan_cut || cut,
        bytes_read,
        started_ms: 0.0,
        planned: fingerprints,
        done: Vec::new(),
        replaces,
        reused,
        text,
        members,
    };
    Ok((
        PlanResponse {
            root,
            files: planned,
            truncated,
        },
        pending,
    ))
}

/// Start a stepped pack of `{ files: [{ relative, id?, size, kind?,
/// modified?, scanned? }], truncated?, replaces?, ...settings }`, where `kind`
/// is the scan's kind for the file, `size` and `modified` are the file as the
/// page just read it, `scanned` the size the scan listed it at, `truncated`
/// says the scan was cut at its budgets, and `replaces` names the result on
/// screen. Returns the grant root and the files
/// to extract, in order, with their names inside the root, their kinds,
/// whether each goes alone, and whether it is `cached`: taken from the
/// replaced result, so the page neither reads nor extracts it. The files are
/// ticked already; nothing else is selected.
#[wasm_bindgen]
pub fn pack_begin(input: JsValue) -> Result<JsValue, JsValue> {
    let started_ms = js_now_ms();
    let settings: Settings = from_js(input.clone(), "pack")?;
    let req: PlanInput = from_js(input, "pack")?;
    let (resp, mut pending) = RESULTS
        .with(|results| {
            let results = results.borrow();
            let previous = req
                .replaces
                .as_deref()
                .and_then(|id| results.peek(id))
                .and_then(|stored| stored.reuse.as_ref());
            plan(req, &settings, previous)
        })
        .map_err(|e| JsValue::from_str(&e))?;
    pending.started_ms = started_ms;
    PENDING.with(|slot| *slot.borrow_mut() = Some(pending));
    to_js(&resp)
}

/// Drop the pack [`pack_begin`] started, as a cancelled or failed pack
/// leaves it. The result it would have replaced stays as it was.
#[wasm_bindgen]
pub fn pack_abort() {
    PENDING.with(|slot| slot.borrow_mut().take());
}

/// One extracted file between instances. Kinds and statuses travel as the
/// labels [`Kind::as_str`] and [`FileStatus::as_str`] give them.
#[derive(Serialize, Deserialize)]
struct WireFile {
    id: String,
    relative: String,
    kind: String,
    size: u64,
    text: String,
    status: String,
    /// The parser's message for an error or an unreadable file.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    detail: String,
    /// The per-file cap a file over it exceeded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    limit: Option<u64>,
}

impl From<PackedFile> for WireFile {
    fn from(file: PackedFile) -> Self {
        let status = file.status.as_str().to_string();
        let (detail, limit) = match file.status {
            FileStatus::Error(message) | FileStatus::Unreadable(message) => (message, None),
            FileStatus::TooLarge(limit) => (String::new(), Some(limit)),
            _ => (String::new(), None),
        };
        Self {
            id: file.id,
            relative: file.relative,
            kind: file.kind.as_str().to_string(),
            size: file.size,
            text: file.text,
            status,
            detail,
            limit,
        }
    }
}

impl WireFile {
    fn into_packed(self) -> Result<PackedFile, String> {
        let status = match self.status.as_str() {
            "extracted" => FileStatus::Extracted,
            "skipped_binary" => FileStatus::SkippedBinary,
            "too_large" => FileStatus::TooLarge(self.limit.unwrap_or(0)),
            "skipped_archive" => FileStatus::SkippedArchive,
            "changed" => FileStatus::Changed,
            "error" => FileStatus::Error(self.detail),
            "unreadable" => FileStatus::Unreadable(self.detail),
            other => return Err(format!("unknown file status {other}")),
        };
        let kind = kind_from_label(&self.kind)
            .ok_or_else(|| format!("unknown file kind {}", self.kind))?;
        Ok(PackedFile {
            id: self.id,
            relative: self.relative,
            kind,
            size: self.size,
            text: self.text,
            status,
        })
    }
}

/// A batch of extracted files and whether an archive in it was cut short.
#[derive(Serialize, Deserialize)]
struct Extracted {
    files: Vec<WireFile>,
    #[serde(default)]
    truncated: bool,
    /// The files the batch extracted and the entries each became. A file
    /// noted for a failure is left out, so a later pack tries it again.
    #[serde(default)]
    inputs: Vec<InputDone>,
}

/// A ticked file a batch extracted, and the entries it became.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct InputDone {
    id: String,
    /// Ids of its entries: its own, or the members of its archive.
    entries: Vec<String>,
    /// Its archive stopped at the archive budget.
    #[serde(default)]
    cut: bool,
}

/// One file of a batch: its bytes, or why there are none.
#[derive(Deserialize)]
struct ExtractFileIn {
    /// Path inside the grant root, as [`pack_begin`] named it.
    relative: String,
    #[serde(default)]
    id: String,
    #[serde(default)]
    size: u64,
    #[serde(default, deserialize_with = "bytes::deserialize")]
    bytes: Vec<u8>,
    /// The kind [`pack_begin`] planned the file as, for its note if it fails.
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    failure: Option<Failure>,
}

#[derive(Deserialize)]
struct ExtractInput {
    #[serde(default)]
    root: String,
    #[serde(default)]
    files: Vec<ExtractFileIn>,
}

/// The dump entry for a file with no text. A file the browser no longer
/// reads is `changed`, as `pulp ui` marks a file that changed after its scan,
/// with the reason it gives, if any (`no longer there`); anything else is an
/// extraction error, noted as `pulp ui` notes one, with the name written the
/// way the dump writes names.
fn failed_file(
    id: String,
    relative: String,
    size: u64,
    kind: Kind,
    failure: Failure,
) -> PackedFile {
    let message = match failure.reason.as_str() {
        "changed" => {
            let why = failure.message.trim();
            return PackedFile {
                id,
                relative,
                kind,
                size,
                text: if why.is_empty() {
                    format!("[{CHANGED_NOTE}]")
                } else {
                    format!("[{CHANGED_NOTE}: {why}]")
                },
                status: FileStatus::Changed,
            };
        }
        "timeout" => format!("extractor timed out after {EXTRACT_TIMEOUT_SECS}s"),
        "no_worker" => NO_WORKER_MESSAGE.to_string(),
        _ if failure.message.trim().is_empty() => "the file could not be read".to_string(),
        _ => failure.message,
    };
    PackedFile {
        text: format!(
            "[error extracting {}: {message}]",
            pulp::tree::display_path(&relative)
        ),
        id,
        relative,
        kind,
        size,
        status: FileStatus::Error(message),
    }
}

/// Extract one batch the way [`pack_entries`] extracts it. The batch was
/// planned by [`pack_begin`], so its filters and budgets are spent already.
fn extract_batch(req: ExtractInput, settings: &Settings) -> Result<Extracted, String> {
    let root = if req.root.is_empty() {
        ".".to_string()
    } else {
        req.root
    };
    let opts = Options {
        max_total_bytes: u64::MAX,
        max_entries: 0,
        tree: TreeMode::None,
        ..settings.options(&root)
    };
    let mut files = Vec::new();
    let mut mem = Vec::new();
    let mut ids = Vec::new();
    for f in &req.files {
        let id = if f.id.is_empty() {
            f.relative.clone()
        } else {
            f.id.clone()
        };
        ids.push(id);
    }
    for (f, id) in req.files.iter().zip(&ids) {
        match &f.failure {
            Some(failure) => files.push(WireFile::from(failed_file(
                id.clone(),
                f.relative.clone(),
                f.size,
                known_kind(f.kind.as_deref(), &f.relative),
                failure.clone(),
            ))),
            None => mem.push(MemoryFile {
                id,
                relative: &f.relative,
                bytes: &f.bytes,
            }),
        }
    }
    let mut truncated = false;
    let mut inputs = Vec::new();
    if !mem.is_empty() {
        let packed =
            pack_entries(&mem, &opts, None).map_err(|e| format!("pack_entries failed: {e}"))?;
        truncated = packed.stats.truncated;
        let extracted: Vec<&str> = mem.iter().map(|m| m.id).collect();
        inputs = attribute(&extracted, &packed.files, truncated);
        files.extend(packed.files.into_iter().map(WireFile::from));
    }
    Ok(Extracted {
        files,
        truncated,
        inputs,
    })
}

/// Which extracted file each entry of a batch came from: the file with its
/// id, or the archive whose id and a `!` start it. A batch whose entries
/// cannot all be placed, or that an archive cut short while it held more than
/// one file, reports none, so none of its files is taken by a later pack.
fn attribute(ids: &[&str], entries: &[PackedFile], cut: bool) -> Vec<InputDone> {
    let mut owned: Vec<Vec<String>> = vec![Vec::new(); ids.len()];
    if ids.len() == 1 {
        owned[0] = entries.iter().map(|e| e.id.clone()).collect();
    } else {
        if cut {
            return Vec::new();
        }
        let at: HashMap<&str, usize> = ids.iter().enumerate().map(|(i, id)| (*id, i)).collect();
        for entry in entries {
            let owner = at.get(entry.id.as_str()).copied().or_else(|| {
                ids.iter()
                    .enumerate()
                    .filter(|(_, id)| {
                        entry
                            .id
                            .strip_prefix(**id)
                            .is_some_and(|rest| rest.starts_with('!'))
                    })
                    .max_by_key(|(_, id)| id.len())
                    .map(|(i, _)| i)
            });
            match owner {
                Some(i) => owned[i].push(entry.id.clone()),
                None => return Vec::new(),
            }
        }
    }
    ids.iter()
        .zip(owned)
        .map(|(id, entries)| InputDone {
            id: (*id).to_string(),
            entries,
            cut,
        })
        .collect()
}

/// Extract a batch of a stepped pack: `{ root, files: [{ relative, id?, size,
/// kind?, bytes | failure }], ...settings }`. Returns the batch as JSON bytes
/// (a `Uint8Array`) for [`pack_add`]: an archive can expand to more text than
/// one JS string holds. A file with a `failure` ({ reason, message }) gets the
/// note its failure calls for. Stores nothing, so it can run in a spare
/// instance.
#[wasm_bindgen]
pub fn extract_files(input: JsValue) -> Result<Vec<u8>, JsValue> {
    let settings: Settings = from_js(input.clone(), "extract")?;
    let req: ExtractInput = from_js(input, "extract")?;
    let batch = extract_batch(req, &settings).map_err(|e| JsValue::from_str(&e))?;
    serde_json::to_vec(&batch).map_err(|e| JsValue::from_str(&e.to_string()))
}

fn add(pending: &mut Pending, json: &[u8]) -> Result<(), String> {
    let batch: Extracted =
        serde_json::from_slice(json).map_err(|e| format!("invalid extracted batch: {e}"))?;
    let files = batch
        .files
        .into_iter()
        .map(WireFile::into_packed)
        .collect::<Result<Vec<_>, _>>()?;
    pending.truncated |= batch.truncated;
    pending.done.extend(batch.inputs);
    pending.files.extend(files);
    Ok(())
}

/// Add a batch from [`extract_files`], its JSON bytes, to the pack
/// [`pack_begin`] started. A batch that fails to add leaves the pack as it
/// was, so its files can be noted instead.
#[wasm_bindgen]
pub fn pack_add(json: &[u8]) -> Result<(), JsValue> {
    PENDING
        .with(|slot| match slot.borrow_mut().as_mut() {
            Some(pending) => add(pending, json),
            None => Err("no pack is running; pulp again".to_string()),
        })
        .map_err(|e| JsValue::from_str(&e))
}

/// The collected files as one result: sorted, mapped, and counted the way
/// [`pack_entries`] leaves them, with the record a later pack reuses it by.
fn finish(pending: Pending) -> StoredResult {
    let Pending {
        opts,
        no_tree,
        mut files,
        truncated,
        bytes_read,
        planned,
        done,
        text,
        members,
        ..
    } = pending;
    files.sort_by(|a, b| a.relative.cmp(&b.relative));
    let reuse = record(&files, &planned, done, text, members);
    let extracted: Vec<String> = files
        .iter()
        .filter(|f| f.status == FileStatus::Extracted)
        .map(|f| f.relative.clone())
        .collect();
    let tree = pulp::tree::render_tree(&pulp::pack::tree_label(&opts.roots), &extracted);
    let files_extracted = extracted.len();
    let files_skipped = files.len().saturating_sub(files_extracted);
    let (chars_emitted, tokens_est) = pulp::tokens::summarize_chunks(
        std::iter::once(tree.as_str()).chain(
            files
                .iter()
                .filter(|f| f.status == FileStatus::Extracted)
                .map(|f| f.text.as_str()),
        ),
    );
    StoredResult {
        packed: Packed {
            files,
            tree,
            stats: Stats {
                files_extracted,
                files_skipped,
                bytes_read,
                chars_emitted,
                tokens_est,
                truncated,
                cancelled: false,
                ..Stats::default()
            },
        },
        format: opts.format,
        tree: !no_tree,
        source_mode: opts.source_mode,
        extract_ms: 0.0,
        reuse: Some(reuse),
    }
}

/// What a later pack may take from a result of `files`: every file the pack
/// extracted from bytes the page gave a modification time for, with the
/// positions its entries now hold.
fn record(
    files: &[PackedFile],
    planned: &HashMap<String, Fingerprint>,
    done: Vec<InputDone>,
    text: TextSettings,
    members: MemberSettings,
) -> Reusable {
    // An id twice over cannot say which entry is whose; neither is recorded.
    let mut at: HashMap<&str, Option<usize>> = HashMap::with_capacity(files.len());
    for (i, file) in files.iter().enumerate() {
        at.entry(file.id.as_str())
            .and_modify(|slot| *slot = None)
            .or_insert(Some(i));
    }
    let mut inputs = HashMap::with_capacity(done.len());
    for input in done {
        let Some(fingerprint) = planned.get(&input.id) else {
            continue;
        };
        let Some(modified) = fingerprint.modified else {
            continue;
        };
        let Some(outputs) = input
            .entries
            .iter()
            .map(|entry| at.get(entry.as_str()).copied().flatten())
            .collect::<Option<Vec<usize>>>()
        else {
            continue;
        };
        let archive = match outputs.as_slice() {
            [only] => files[*only].id != input.id || files[*only].kind.is_archive(),
            _ => true,
        };
        inputs.insert(
            input.id,
            Extraction {
                relative: fingerprint.relative.clone(),
                size: fingerprint.size,
                modified,
                outputs,
                archive,
                cut: input.cut,
            },
        );
    }
    Reusable {
        text,
        members,
        inputs,
    }
}

/// Move the entries of the files this pack takes from `old`, the result it
/// replaces, into the pack. `old` goes with whatever else it held.
fn take_reused(pending: &mut Pending, old: Option<StoredResult>) -> Result<(), String> {
    if pending.reused.is_empty() {
        return Ok(());
    }
    let old = old.ok_or_else(|| RESULT_GONE.to_string())?;
    let mut slots: Vec<Option<PackedFile>> = old.packed.files.into_iter().map(Some).collect();
    for (id, extraction) in std::mem::take(&mut pending.reused) {
        let mut entries = Vec::with_capacity(extraction.outputs.len());
        for &at in &extraction.outputs {
            let file = slots
                .get_mut(at)
                .and_then(Option::take)
                .ok_or_else(|| RESULT_GONE.to_string())?;
            entries.push(file.id.clone());
            pending.files.push(file);
        }
        pending.planned.insert(
            id.clone(),
            Fingerprint {
                relative: extraction.relative,
                size: extraction.size,
                modified: Some(extraction.modified),
            },
        );
        pending.done.push(InputDone {
            id,
            entries,
            cut: extraction.cut,
        });
    }
    Ok(())
}

/// The pack [`pack_begin`] started, with the files it takes from the result
/// it replaces, which goes from the store.
fn complete(mut pending: Pending) -> Result<StoredResult, String> {
    if let Some(old) = pending.replaces.take() {
        let old = RESULTS.with(|results| results.borrow_mut().take(&old));
        take_reused(&mut pending, old)?;
    }
    Ok(finish(pending))
}

/// End the pack [`pack_begin`] started: store it as a result and return what
/// [`pack_files`] returns. The result it replaces is dropped.
#[wasm_bindgen]
pub fn pack_finish() -> Result<JsValue, JsValue> {
    let pending = PENDING
        .with(|slot| slot.borrow_mut().take())
        .ok_or_else(|| JsValue::from_str("no pack is running; pulp again"))?;
    let started_ms = pending.started_ms;
    let mut stored = complete(pending).map_err(|e| JsValue::from_str(&e))?;
    stored.extract_ms = js_now_ms() - started_ms;
    let mut resp = store_and_respond(stored).map_err(|e| JsValue::from_str(&e))?;
    resp.elapsed_ms = elapsed_since(started_ms);
    to_js(&resp)
}

/// Redraw a stored result, `{ result_id, format, no_tree }`, without extracting
/// again. Returns what [`pack_files`] returns, under the same `result_id`.
#[wasm_bindgen]
pub fn render_result(input: JsValue) -> Result<JsValue, JsValue> {
    let t0 = js_now_ms();
    let req: RenderInput = from_js(input, "render")?;
    let format = parse_format(&req.format);
    let resp = with_result(&req.result_id, |stored| {
        let mut resp = respond(stored, &req.result_id, format, !req.no_tree)?;
        resp.elapsed_ms = (stored.extract_ms + js_now_ms() - t0).max(0.0) as u64;
        Ok(resp)
    })?;
    to_js(&resp)
}

/// Full dump for a stored result, `{ result_id, format, no_tree }`.
#[wasm_bindgen]
pub fn artifact_as(input: JsValue) -> Result<String, JsValue> {
    let req: RenderInput = from_js(input, "artifact")?;
    with_result(&req.result_id, |stored| {
        stored.render(parse_format(&req.format), !req.no_tree)
    })
}

/// Bytes of the full dump handed to JS at a time by [`artifact_chunks`].
const DUMP_CHUNK_BYTES: usize = 8 * 1024 * 1024;

/// Hands the bytes written to it to `sink` in chunks of [`DUMP_CHUNK_BYTES`].
struct Chunks<'a> {
    sink: &'a mut dyn FnMut(&[u8]) -> std::io::Result<()>,
    buf: Vec<u8>,
    total: usize,
}

impl Chunks<'_> {
    fn emit(&mut self) -> std::io::Result<()> {
        if !self.buf.is_empty() {
            (self.sink)(&self.buf)?;
            self.buf.clear();
        }
        Ok(())
    }
}

impl std::io::Write for Chunks<'_> {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.buf.extend_from_slice(data);
        self.total = self.total.saturating_add(data.len());
        if self.buf.len() >= DUMP_CHUNK_BYTES {
            self.emit()?;
        }
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.emit()
    }
}

/// Write the full dump of a stored result to `sink` in chunks, so a dump
/// larger than one JS string can be saved. Returns its length in bytes.
fn dump_chunks(
    stored: &mut StoredResult,
    format: OutputFormat,
    tree: bool,
    sink: &mut dyn FnMut(&[u8]) -> std::io::Result<()>,
) -> Result<usize, String> {
    let mut out = Chunks {
        sink,
        buf: Vec::with_capacity(DUMP_CHUNK_BYTES),
        total: 0,
    };
    stored.write(&mut out, format, tree)?;
    out.emit().map_err(|e| format!("render failed: {e}"))?;
    Ok(out.total)
}

/// Full dump for a stored result, `{ result_id, format, no_tree }`, passed to
/// `sink` as `Uint8Array` chunks of UTF-8. Returns the dump's length in bytes.
#[wasm_bindgen]
pub fn artifact_chunks(input: JsValue, sink: &js_sys::Function) -> Result<f64, JsValue> {
    let req: RenderInput = from_js(input, "artifact")?;
    let mut send = |bytes: &[u8]| {
        sink.call1(&JsValue::NULL, &js_sys::Uint8Array::from(bytes))
            .map(|_| ())
            .map_err(|_| std::io::Error::other("the dump could not be handed over"))
    };
    let total = with_result(&req.result_id, |stored| {
        dump_chunks(stored, parse_format(&req.format), !req.no_tree, &mut send)
    })?;
    Ok(total as f64)
}

/// Forget a stored result. Returns whether it was still held.
#[wasm_bindgen]
pub fn drop_result(result_id: &str) -> bool {
    RESULTS.with(|results| results.borrow_mut().remove(result_id))
}

/// The text pulp extracts from one file, capped for display. Stores nothing.
fn preview(req: PreviewInput, settings: &Settings) -> Result<PreviewResponse, String> {
    let relative = normalize_rel(&req.relative);
    if relative.is_empty() {
        return Err("preview needs a file path".into());
    }
    if let Some(failure) = req.failure {
        let size = req.size.unwrap_or(0);
        let kind = known_kind(req.kind.as_deref(), &relative);
        let file = failed_file(relative.clone(), relative, size, kind, failure);
        return Ok(PreviewResponse {
            text: String::new(),
            truncated: false,
            status: file.status.as_str(),
            message: status_message(&file.status, size),
            kind: file.kind.as_str(),
            language: language_name(Path::new(&file.relative), file.kind),
        });
    }
    let kind = classify(Path::new(&relative), Some(&req.bytes));
    let language = language_name(Path::new(&relative), kind);
    let mut stored = extract(
        PackInput {
            files: vec![PackFileIn {
                relative: relative.clone(),
                id: String::new(),
                bytes: req.bytes,
            }],
            selected: vec![relative.clone()],
        },
        settings,
    )?;

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
        let message = status_message(&file.status, file.size);
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
                status_message(&files[0].status, files[0].size),
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
/// `hidden`, ...), capped at 32 KiB. A file the browser could not read comes
/// as `{ relative, size, failure: { reason, message } }` and gets the status
/// and message a pack would give it. Stores nothing.
#[wasm_bindgen]
pub fn preview_file(input: JsValue) -> Result<JsValue, JsValue> {
    let settings: Settings = from_js(input.clone(), "preview")?;
    let req: PreviewInput = from_js(input, "preview")?;
    let resp = preview(req, &settings).map_err(|e| JsValue::from_str(&e))?;
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

    fn settings(format: &str, no_tree: bool) -> Settings {
        Settings {
            format: format.to_string(),
            no_tree,
            ..Settings::default()
        }
    }

    fn input(files: Vec<PackFileIn>) -> PackInput {
        PackInput {
            selected: files.iter().map(|f| f.relative.clone()).collect(),
            files,
        }
    }

    fn extract_all(files: Vec<PackFileIn>, format: &str, no_tree: bool) -> StoredResult {
        extract(input(files), &settings(format, no_tree)).unwrap()
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
        extract_all(vec![file("a.rs", b"fn a() {}\n")], "txt", false)
    }

    fn preview_of(relative: &str, body: &[u8]) -> PreviewInput {
        PreviewInput {
            relative: relative.to_string(),
            bytes: body.to_vec(),
            ..PreviewInput::default()
        }
    }

    fn preview_default(req: PreviewInput) -> PreviewResponse {
        preview(req, &Settings::default()).unwrap()
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

    /// A zip holding `entries`, stored without compression.
    fn zip_of(entries: &[(&str, &[u8])]) -> Vec<u8> {
        // Local headers, then the central directory, then its end record.
        let mut out = Vec::new();
        let mut central = Vec::new();
        for (name, data) in entries {
            let crc = crc32(data);
            let offset = out.len() as u32;
            let header = |sig: u32, central: bool| {
                let mut h = Vec::new();
                h.extend_from_slice(&sig.to_le_bytes());
                if central {
                    h.extend_from_slice(&20u16.to_le_bytes());
                }
                h.extend_from_slice(&20u16.to_le_bytes());
                h.extend_from_slice(&0u16.to_le_bytes());
                h.extend_from_slice(&0u16.to_le_bytes());
                h.extend_from_slice(&0u32.to_le_bytes());
                h.extend_from_slice(&crc.to_le_bytes());
                h.extend_from_slice(&(data.len() as u32).to_le_bytes());
                h.extend_from_slice(&(data.len() as u32).to_le_bytes());
                h.extend_from_slice(&(name.len() as u16).to_le_bytes());
                h.extend_from_slice(&0u16.to_le_bytes());
                if central {
                    // Comment length, disk, internal and external attributes.
                    h.extend_from_slice(&[0; 10]);
                    h.extend_from_slice(&offset.to_le_bytes());
                }
                h.extend_from_slice(name.as_bytes());
                h
            };
            out.extend(header(0x0403_4b50, false));
            out.extend_from_slice(data);
            central.extend(header(0x0201_4b50, true));
        }
        let at = out.len() as u32;
        let size = central.len() as u32;
        out.extend(central);
        out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
        out.extend_from_slice(&[0; 4]);
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        out.extend_from_slice(&size.to_le_bytes());
        out.extend_from_slice(&at.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out
    }

    fn crc32(data: &[u8]) -> u32 {
        let mut crc = 0xffff_ffffu32;
        for &b in data {
            crc ^= u32::from(b);
            for _ in 0..8 {
                crc = if crc & 1 == 1 {
                    (crc >> 1) ^ 0xedb8_8320
                } else {
                    crc >> 1
                };
            }
        }
        !crc
    }

    /// Everything a stepped pack must reproduce: a grant with hidden, excluded,
    /// oversized, binary, damaged, and archived files.
    fn awkward() -> Vec<PackFileIn> {
        let mut files = project();
        files.extend([
            file("tides/.github/ci.yml", b"on: push\n"),
            file("tides/node_modules/pkg/index.js", b"module.exports = 1;\n"),
            file("tides/.env", b"SECRET=1\n"),
            file("tides/docs/paper.pdf", b"%PDF-1.4 garbage"),
            file("tides/notes/empty.txt", b""),
            file("tides/notes/blob.xyzzy", &[0, 1, 2, 3]),
            file("tides/big.txt", &vec![b'x'; 2048]),
            file(
                "tides/data/bundle.zip",
                &zip_of(&[
                    ("a.rs", b"fn a() {}\n"),
                    (".hidden", b"h\n"),
                    ("inner/b.md", b"# b\n"),
                ]),
            ),
        ]);
        files
    }

    /// Settings a stepped pack is checked under: a small per-file cap so
    /// `big.txt` is over it.
    fn awkward_settings(archives: bool, hidden: bool, source: bool) -> Settings {
        Settings {
            archives,
            hidden,
            source,
            max_file_size: Some(1024),
            ..settings("xml", false)
        }
    }

    /// A file to plan, named `relative`, `size` bytes long, of no known kind.
    fn plan_file(relative: &str, size: u64) -> PlanFileIn {
        PlanFileIn {
            relative: relative.to_string(),
            id: String::new(),
            size,
            kind: None,
            modified: None,
            scanned: None,
        }
    }

    fn plan_of(
        files: Vec<PlanFileIn>,
        settings: &Settings,
    ) -> Result<(PlanResponse, Pending), String> {
        plan(
            PlanInput {
                files,
                truncated: false,
                replaces: None,
            },
            settings,
            None,
        )
    }

    /// A batch file of `relative` with `bytes`, or with a `failure` instead.
    fn batch_file(
        relative: &str,
        id: &str,
        bytes: &[u8],
        failure: Option<Failure>,
    ) -> ExtractFileIn {
        ExtractFileIn {
            relative: relative.to_string(),
            id: id.to_string(),
            size: bytes.len() as u64,
            bytes: bytes.to_vec(),
            kind: None,
            failure,
        }
    }

    /// One batch through [`extract_batch`] and JSON bytes into `pending`.
    fn add_batch(
        pending: &mut Pending,
        root: &str,
        files: Vec<ExtractFileIn>,
        settings: &Settings,
    ) {
        let req = ExtractInput {
            root: root.to_string(),
            files,
        };
        let json = serde_json::to_vec(&extract_batch(req, settings).unwrap()).unwrap();
        add(pending, &json).unwrap();
    }

    /// Run `files` through the stepped pack in batches of `batch` files, the
    /// way the mill's worker runs it, with every batch through JSON.
    fn stepped(files: &[PackFileIn], settings: &Settings, batch: usize) -> StoredResult {
        let planned: Vec<PlanFileIn> = files
            .iter()
            .map(|f| PlanFileIn {
                id: f.id.clone(),
                ..plan_file(&f.relative, f.bytes.len() as u64)
            })
            .collect();
        let (resp, mut pending) = plan_of(planned, settings).unwrap();
        for chunk in resp.files.chunks(batch) {
            let batch = chunk
                .iter()
                .map(|p| batch_file(&p.relative, &p.id, &files[p.index as usize].bytes, None))
                .collect();
            add_batch(&mut pending, &resp.root, batch, settings);
        }
        finish(pending)
    }

    fn assert_same_result(got: &mut StoredResult, want: &mut StoredResult, case: &str) {
        for format in FORMATS {
            for tree in [true, false] {
                let got = respond(got, "w-1", format, tree).unwrap();
                let want = respond(want, "w-1", format, tree).unwrap();
                let case = format!("{case} {format:?} tree={tree}");
                assert_eq!(got.dump, want.dump, "{case}");
                assert_eq!(got.dump_bytes, want.dump_bytes, "{case}");
                assert_eq!(
                    (got.files_extracted, got.files_skipped, got.tokens_est),
                    (want.files_extracted, want.files_skipped, want.tokens_est),
                    "{case}"
                );
                assert_eq!(got.truncated, want.truncated, "{case}");
                let outcome = |o: &FileOutcome| {
                    format!(
                        "{} {} {} {} {} {}",
                        o.id, o.relative, o.status, o.kind, o.size, o.message
                    )
                };
                assert_eq!(
                    got.outcomes.iter().map(outcome).collect::<Vec<_>>(),
                    want.outcomes.iter().map(outcome).collect::<Vec<_>>(),
                    "{case}"
                );
            }
        }
        assert_eq!(got.packed.tree, want.packed.tree, "{case}");
        assert_eq!(got.packed.files, want.packed.files, "{case}");
        assert_eq!(got.format, want.format, "{case}");
        assert_eq!(got.tree, want.tree, "{case}");
    }

    #[test]
    fn test_respond_with_each_format_and_map_matches_fresh_pack() {
        let (_, stats) = fresh(OutputFormat::Plain, true, false);
        assert_eq!((stats.files_extracted, stats.files_skipped), (3, 1));
        for source in [false, true] {
            for packed_with_map in [true, false] {
                let settings = Settings {
                    source,
                    ..settings("xml", !packed_with_map)
                };
                let mut stored = extract(input(project()), &settings).unwrap();
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
        let mut stored = extract_all(project(), "xml", false);
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
        let mut stored = extract_all(project(), "md", true);
        assert_eq!(stored.format, OutputFormat::Markdown);
        assert!(!stored.tree);
        let full = stored.render(stored.format, stored.tree).unwrap();
        assert_eq!(full, fresh(OutputFormat::Markdown, false, false).0);
    }

    #[test]
    fn test_respond_with_large_dump_caps_preview_like_local_mill() {
        let body = "x".repeat(DUMP_PREVIEW_BYTES + 1000) + "\n";
        let mut stored = extract_all(vec![file("big.txt", body.as_bytes())], "txt", false);
        let resp = respond(&mut stored, "w-1", OutputFormat::Plain, true).unwrap();
        assert!(resp.preview_truncated);
        assert_eq!(resp.dump.len(), DUMP_PREVIEW_BYTES, "no note is added");
        let full = stored.render(OutputFormat::Plain, true).unwrap();
        assert_eq!(full.len(), resp.dump_bytes);
        assert!(full.starts_with(&resp.dump));
        assert!(full.contains(&body));
    }

    #[test]
    fn test_respond_with_multibyte_text_caps_preview_on_char_boundary() {
        let body = "€".repeat(DUMP_PREVIEW_BYTES);
        let mut stored = extract_all(vec![file("euro.txt", body.as_bytes())], "txt", true);
        let resp = respond(&mut stored, "w-1", OutputFormat::Plain, false).unwrap();
        assert!(resp.preview_truncated);
        assert!(resp.dump.len() <= DUMP_PREVIEW_BYTES);
        assert!(resp.dump.len() > DUMP_PREVIEW_BYTES - 3);
    }

    #[test]
    fn test_render_head_matches_the_capped_full_dump() {
        let mut stored = extract_all(
            vec![
                file("tides/a.md", "é€𝄞 tides\n".repeat(40).as_bytes()),
                file("tides/b.rs", b"fn b() {}\n"),
            ],
            "md",
            false,
        );
        for format in FORMATS {
            for tree in [true, false] {
                let full = stored.render(format, tree).unwrap();
                for max in [
                    0,
                    1,
                    7,
                    8,
                    9,
                    10,
                    11,
                    100,
                    333,
                    full.len() - 1,
                    full.len(),
                    full.len() + 5,
                ] {
                    let (head, total) = stored.render_head(format, tree, max).unwrap();
                    assert_eq!(total, full.len(), "{format:?} {tree} {max}");
                    assert_eq!(
                        head,
                        cap_text(full.clone(), max).0,
                        "{format:?} {tree} {max}"
                    );
                }
            }
        }
    }

    #[test]
    fn test_dump_chunks_hand_over_the_whole_dump_in_chunks() {
        // Two files under the per-file cap that together outgrow one chunk.
        let body = "x".repeat(DUMP_CHUNK_BYTES / 2) + "\n" + &"€".repeat(1000);
        let mut stored = extract_all(
            vec![
                file("tides/a.txt", body.as_bytes()),
                file("tides/b.txt", body.as_bytes()),
            ],
            "txt",
            false,
        );
        let full = stored.render(OutputFormat::Plain, true).unwrap();
        let mut parts: Vec<Vec<u8>> = Vec::new();
        let total = dump_chunks(
            &mut stored,
            OutputFormat::Plain,
            true,
            &mut |bytes: &[u8]| {
                parts.push(bytes.to_vec());
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(total, full.len());
        assert!(parts.len() >= 2, "{}", parts.len());
        assert!(parts.iter().all(|p| !p.is_empty()));
        assert_eq!(parts.concat(), full.as_bytes());
    }

    #[test]
    fn test_respond_with_nothing_extracted_returns_error() {
        let mut stored = extract_all(vec![file("logo.png", &[0, 1, 2, 0])], "xml", false);
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
        let off = extract_all(files(), "txt", true);
        assert_eq!(off.packed.stats.files_extracted, 1);
        let on = extract(
            input(files()),
            &Settings {
                hidden: true,
                ..settings("txt", true)
            },
        )
        .unwrap();
        assert_eq!(on.packed.stats.files_extracted, 2);
    }

    #[test]
    fn test_extract_with_partial_selection_packs_only_ticked_files() {
        let files = project();
        let req = PackInput {
            selected: vec!["tides/README.md".into(), "src/lib.rs".into()],
            files,
        };
        let stored = extract(req, &settings("txt", false)).unwrap();
        let packed: Vec<&str> = stored
            .packed
            .files
            .iter()
            .map(|f| f.relative.as_str())
            .collect();
        assert_eq!(
            packed,
            ["README.md", "src/lib.rs"],
            "by id and by inner path"
        );
    }

    #[test]
    fn test_extract_with_empty_selection_returns_empty_pack() {
        let req = PackInput {
            selected: Vec::new(),
            files: project(),
        };
        let stored = extract(req, &settings("txt", false)).unwrap();
        assert!(stored.packed.files.is_empty());
        assert!(stored.packed.tree.is_empty());
    }

    #[test]
    fn test_extract_with_extra_exclude_keeps_default_excludes() {
        let files = vec![
            file("app/.env", b"SECRET=1\n"),
            file("app/keys/id.key", b"PRIVATE\n"),
            file("app/notes.log", b"log\n"),
            file("app/main.rs", b"fn main() {}\n"),
        ];
        let settings = Settings {
            hidden: true,
            exclude: vec!["*.log".into()],
            ..settings("txt", true)
        };
        let stored = extract(input(files), &settings).unwrap();
        let packed: Vec<&str> = stored
            .packed
            .files
            .iter()
            .map(|f| f.relative.as_str())
            .collect();
        assert_eq!(packed, ["main.rs"]);
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
        let got = preview_default(preview_of("tides/src/lib.rs", b"pub fn tide() {}\n"));
        assert_eq!(got.text, "pub fn tide() {}\n");
        assert_eq!(got.status, "extracted");
        assert!(got.message.is_empty());
        assert!(!got.truncated);
        assert_eq!(got.kind, "text");
    }

    #[test]
    fn test_preview_with_long_text_caps_on_char_boundary() {
        let body = "€".repeat(FILE_PREVIEW_BYTES);
        let got = preview_default(preview_of("notes.txt", body.as_bytes()));
        assert!(got.truncated);
        assert_eq!(got.text.len(), FILE_PREVIEW_BYTES - FILE_PREVIEW_BYTES % 3);
        assert!(got.text.chars().all(|c| c == '€'));
    }

    #[test]
    fn test_preview_with_binary_returns_reason_without_text() {
        let got = preview_default(preview_of("assets/logo.png", &[0, 159, 146, 150, 0, 1]));
        assert_eq!(got.status, "skipped_binary");
        assert!(got.text.is_empty());
        assert!(got.message.contains("binary"), "{}", got.message);
    }

    #[test]
    fn test_preview_with_hidden_file_follows_hidden_setting() {
        let off = preview_default(preview_of("app/.github/ci.yml", b"on: push\n"));
        assert!(off.text.is_empty());
        assert!(!off.message.is_empty());
        let on = preview(
            preview_of("app/.github/ci.yml", b"on: push\n"),
            &Settings {
                hidden: true,
                ..Settings::default()
            },
        )
        .unwrap();
        assert_eq!(on.text, "on: push\n");
    }

    #[test]
    fn test_preview_with_sample_pdf_returns_field_notes() {
        let pdf = sample_file("docs/field-notes.pdf");
        let got = preview_default(preview_of("tides/docs/field-notes.pdf", &pdf));
        assert_eq!(got.status, "extracted");
        assert_eq!(got.kind, "pdf");
        assert!(got.text.contains("Field notes"), "{}", got.text);
    }

    #[test]
    fn test_preview_with_sample_archive_follows_archives_setting() {
        let zip = sample_file("data/archive.zip");
        let off = preview_default(preview_of("tides/data/archive.zip", &zip));
        assert_eq!(off.status, "skipped_archive");
        assert!(off.text.is_empty());
        assert!(!off.message.is_empty());
        let on = preview(
            preview_of("tides/data/archive.zip", &zip),
            &Settings {
                archives: true,
                ..Settings::default()
            },
        )
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
    fn test_preview_with_changed_file_returns_choose_again_message() {
        let got = preview_default(PreviewInput {
            relative: "tides/src/lib.rs".into(),
            size: Some(674),
            failure: Some(Failure {
                reason: "changed".into(),
                message: String::new(),
            }),
            ..PreviewInput::default()
        });
        assert_eq!(got.status, "changed");
        assert!(got.text.is_empty());
        assert_eq!(got.message, CHANGED_MESSAGE);
        assert_eq!((got.kind, got.language.as_str()), ("text", "rust"));
    }

    #[test]
    fn test_preview_with_panicked_parser_returns_error_with_size() {
        let got = preview_default(PreviewInput {
            relative: "tides/docs/cjk.pdf".into(),
            size: Some(900),
            failure: Some(Failure {
                reason: "error".into(),
                message: "extractor panicked: unsupported encoding UniJIS-UCS2-H".into(),
            }),
            ..PreviewInput::default()
        });
        assert_eq!(got.status, "error");
        assert_eq!(
            got.message,
            FileStatus::Error("extractor panicked: unsupported encoding UniJIS-UCS2-H".into())
                .message(900)
        );
        assert_eq!(got.kind, "pdf");
    }

    #[test]
    fn test_extract_with_folder_grant_returns_folder_map_and_inner_paths() {
        let mut stored = extract_all(
            vec![
                file("tides/src/lib.rs", b"pub fn tide() {}\n"),
                file("tides/README.md", b"# Tides\n"),
            ],
            "txt",
            false,
        );
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
        let mut stored = extract_all(
            vec![file("a.rs", b"fn a() {}\n"), file("notes/b.md", b"# B\n")],
            "txt",
            false,
        );
        let got = respond(&mut stored, "w-1", OutputFormat::Plain, true).unwrap();
        assert!(
            got.dump.starts_with("Directory structure:\n./\n"),
            "{}",
            got.dump
        );
        assert!(got.dump.contains("FILE: notes/b.md\n"), "{}", got.dump);
    }

    fn scan_input(names: &[(&str, u64)]) -> ScanInput {
        ScanInput {
            files: names
                .iter()
                .map(|(name, size)| ScanFileIn {
                    relative: name.to_string(),
                    size: *size,
                    head: Vec::new(),
                })
                .collect(),
            hidden: false,
            archives: false,
            exclude: Vec::new(),
            no_default_excludes: false,
            max_file_size: None,
            max_total_bytes: None,
            root: None,
        }
    }

    #[test]
    fn test_keep_with_grant_lists_what_the_scan_lists_under_the_same_root() {
        let names = [
            "tides/src/lib.rs",
            "tides/.git/config",
            "tides/node_modules/x/index.js",
            "tides/notes.xyzzy",
            "tides/.env",
            "tides/README.md",
        ];
        let got = keep(KeepInput {
            relatives: names.iter().map(|n| n.to_string()).collect(),
            hidden: false,
            archives: false,
            exclude: Vec::new(),
            no_default_excludes: false,
        })
        .unwrap();
        assert_eq!(got.root, "tides");
        assert_eq!(got.keep, [0, 3, 5]);
        let kept: Vec<(&str, u64)> = got.keep.iter().map(|&i| (names[i as usize], 4)).collect();
        let mut req = scan_input(&kept);
        req.root = Some(got.root.clone());
        let listed: Vec<String> = scan(req).unwrap().files.into_iter().map(|f| f.id).collect();
        assert_eq!(
            listed,
            ["tides/README.md", "tides/notes.xyzzy", "tides/src/lib.rs"]
        );
    }

    #[test]
    fn test_scan_with_named_root_keeps_it_when_the_kept_files_share_a_deeper_folder() {
        // Two dropped folders; the filters keep files from one of them only.
        let got = keep(KeepInput {
            relatives: vec!["proj/src/a.rs".into(), "node_modules/x/b.js".into()],
            hidden: false,
            archives: false,
            exclude: Vec::new(),
            no_default_excludes: false,
        })
        .unwrap();
        assert_eq!((got.root.as_str(), got.keep.as_slice()), (".", &[0][..]));
        let mut req = scan_input(&[("proj/src/a.rs", 4)]);
        req.root = Some(got.root);
        let scanned = scan(req).unwrap();
        assert_eq!(scanned.root, ".");
        assert_eq!(scanned.files[0].relative, "proj/src/a.rs");
    }

    #[test]
    fn test_scan_and_extract_with_hidden_grant_folder_return_its_files() {
        let names = [
            ".dotfiles/zshrc",
            ".dotfiles/.secret",
            ".dotfiles/build/x.rs",
        ];
        let scanned = scan(scan_input(&names.map(|name| (name, 4)))).unwrap();
        let listed: Vec<&str> = scanned.files.iter().map(|f| f.id.as_str()).collect();
        assert_eq!(
            listed,
            [".dotfiles/zshrc"],
            "hidden and build/ inside the folder stay out"
        );

        let stored = extract_all(
            names.iter().map(|name| file(name, b"text\n")).collect(),
            "txt",
            false,
        );
        let packed: Vec<&str> = stored
            .packed
            .files
            .iter()
            .map(|f| f.relative.as_str())
            .collect();
        assert_eq!(packed, ["zshrc"]);
    }

    #[test]
    fn test_scan_with_grant_folder_named_like_generated_output_ticks_its_files() {
        let build = scan(scan_input(&[
            ("build/src/main.rs", 4),
            ("build/Cargo.lock", 4),
        ]))
        .unwrap();
        let ticks: Vec<(&str, bool)> = build
            .files
            .iter()
            .map(|f| (f.id.as_str(), f.default_on))
            .collect();
        assert_eq!(
            ticks,
            [("build/Cargo.lock", false), ("build/src/main.rs", true)],
            "the folder's own name is not judged"
        );
        let venv = scan(scan_input(&[("venv/app.py", 4)])).unwrap();
        assert!(venv.files[0].default_on);
    }

    #[test]
    fn test_scan_with_folder_grant_lists_paths_inside_it() {
        let paths = |got: &ScanResponse| -> Vec<(String, String)> {
            got.files
                .iter()
                .map(|f| (f.id.clone(), f.relative.clone()))
                .collect()
        };
        let folder = scan(scan_input(&[
            ("tides/src/lib.rs", 4),
            ("tides/README.md", 4),
        ]))
        .unwrap();
        assert_eq!(folder.root, "tides");
        assert_eq!(
            paths(&folder),
            [
                ("tides/README.md".into(), "README.md".into()),
                ("tides/src/lib.rs".into(), "src/lib.rs".into())
            ]
        );
        for names in [
            &[("a.rs", 4), ("notes/b.md", 4)][..],
            &[("one/a.rs", 4), ("two/b.rs", 4)],
        ] {
            let got = scan(scan_input(names)).unwrap();
            assert_eq!(got.root, ".", "{names:?}");
            assert!(got.files.iter().all(|f| f.id == f.relative), "{names:?}");
        }
    }

    #[test]
    fn test_extract_with_named_root_keeps_paths_of_a_multi_folder_grant() {
        let settings = Settings {
            root: Some(".".into()),
            ..settings("txt", false)
        };
        let stored = extract(input(vec![file("one/a.rs", b"fn a() {}\n")]), &settings).unwrap();
        assert_eq!(stored.packed.files[0].relative, "one/a.rs");
        assert!(
            stored.packed.tree.starts_with("./\n"),
            "{}",
            stored.packed.tree
        );
        let derived = extract_all(vec![file("one/a.rs", b"fn a() {}\n")], "txt", false);
        assert_eq!(
            derived.packed.files[0].relative, "a.rs",
            "without a root the shared folder is it"
        );
        let (resp, _) = plan_of(vec![plan_file("one/a.rs", 10)], &settings).unwrap();
        assert_eq!(
            (resp.root.as_str(), resp.files[0].relative.as_str()),
            (".", "one/a.rs")
        );
    }

    #[test]
    fn test_scan_and_extract_keep_a_backslash_in_a_file_name() {
        let scanned = scan(scan_input(&[("names/sub/back\\slash.txt", 4)])).unwrap();
        assert_eq!(scanned.files[0].id, "names/sub/back\\slash.txt");
        let stored = extract_all(vec![file("names/sub/back\\slash.txt", b"x\n")], "txt", true);
        assert_eq!(stored.packed.files[0].relative, "sub/back\\slash.txt");
    }

    #[test]
    fn test_scan_over_byte_budget_stops_at_the_first_file_that_does_not_fit() {
        let mut req = scan_input(&[
            ("app/a.rs", 40),
            ("app/b.rs", 40),
            ("app/big.txt", 5000),
            ("app/c.rs", 40),
            ("app/d.rs", 10),
        ]);
        req.max_file_size = Some(1000);
        req.max_total_bytes = Some(100);
        let got = scan(req).unwrap();
        let listed: Vec<(&str, bool)> = got
            .files
            .iter()
            .map(|f| (f.id.as_str(), f.oversized))
            .collect();
        assert_eq!(
            listed,
            [
                ("app/a.rs", false),
                ("app/b.rs", false),
                ("app/big.txt", true)
            ],
            "a file over the per-file cap spends none of the budget; from c.rs on nothing fits"
        );
        assert!(got.truncated);
        assert_eq!(got.bytes, 5080);
        let under = scan(scan_input(&[("app/a.rs", 40), ("app/b.rs", 40)])).unwrap();
        assert!(!under.truncated);
    }

    #[test]
    fn test_scan_spends_budget_in_walk_order() {
        // `a/b.txt` comes before `a-c.txt`: a folder's files before a longer name.
        let mut req = scan_input(&[("g/a-c.txt", 60), ("g/a/b.txt", 60)]);
        req.max_total_bytes = Some(100);
        let got = scan(req).unwrap();
        let listed: Vec<&str> = got.files.iter().map(|f| f.id.as_str()).collect();
        assert_eq!(listed, ["g/a/b.txt"]);
        assert!(got.truncated);
    }

    #[test]
    fn test_scan_with_extra_exclude_keeps_default_excludes() {
        let mut req = scan_input(&[
            ("app/keys/id.key", 4),
            ("app/notes.log", 4),
            ("app/main.rs", 4),
        ]);
        req.exclude = vec!["*.log".into()];
        let listed: Vec<String> = scan(req).unwrap().files.into_iter().map(|f| f.id).collect();
        assert_eq!(listed, ["app/main.rs"]);
    }

    #[test]
    fn test_scan_with_head_sniffs_unknown_extension() {
        let mut req = scan_input(&[("app/notes.xyzzy", 6), ("app/blob.xyzzy", 4)]);
        req.files[0].head = b"hello\n".to_vec();
        req.files[1].head = vec![0, 1, 2, 3];
        let got = scan(req).unwrap();
        let kinds: Vec<(&str, &str, bool)> = got
            .files
            .iter()
            .map(|f| (f.id.as_str(), f.kind, f.default_on))
            .collect();
        assert_eq!(
            kinds,
            [
                ("app/blob.xyzzy", "binary", false),
                ("app/notes.xyzzy", "text", true)
            ]
        );
    }

    #[test]
    fn test_unknown_returns_only_listed_files_named_without_a_kind() {
        let req = scan_input(&[
            ("app/notes.xyzzy", 6),
            ("app/src/lib.rs", 4),
            ("app/node_modules/x/LICENSE_FILE", 4),
            ("app/.cache/blob", 4),
            ("app/README", 4),
            ("app/data", 4),
        ]);
        let got = unknown(&req).unwrap();
        assert_eq!(
            got,
            [0, 5],
            "README is known by name; excluded and hidden files stay out"
        );
    }

    #[test]
    fn test_grant_root_matches_split_grant_root() {
        let cases: [&[&str]; 9] = [
            &["tides/src/lib.rs", "tides/README.md"],
            &["a.rs", "notes/b.md"],
            &["tides/a.rs", "other/b.rs"],
            &["tides/a.rs"],
            &["tides"],
            &[],
            &["", "tides/a.rs", "./tides/b.rs"],
            &["tides\\src\\lib.rs", "tides/README.md"],
            &["../tides/a.rs", "tides/./b.rs"],
        ];
        for case in cases {
            let owned: Vec<String> = case.iter().map(|s| s.to_string()).collect();
            assert_eq!(
                grant_root(owned.iter().map(String::as_str)),
                pulp::tree::split_grant_root(&owned).0,
                "{case:?}"
            );
        }
    }

    #[test]
    fn test_stepped_pack_matches_one_call_pack() {
        let files = awkward();
        for archives in [false, true] {
            for hidden in [false, true] {
                for source in [false, true] {
                    let settings = awkward_settings(archives, hidden, source);
                    let mut want = extract(input(awkward()), &settings).unwrap();
                    let member = want.packed.files.iter().any(|f| {
                        f.relative == "data/bundle.zip/a.rs" && f.status == FileStatus::Extracted
                    });
                    assert_eq!(member, archives, "the zip expands only with archives on");
                    for batch in [1, 3, 64] {
                        let mut got = stepped(&files, &settings, batch);
                        let case = format!(
                            "archives={archives} hidden={hidden} source={source} batch={batch}"
                        );
                        assert_same_result(&mut got, &mut want, &case);
                    }
                }
            }
        }
    }

    #[test]
    fn test_stepped_pack_with_byte_budget_matches_one_call_pack() {
        let files = vec![
            file("app/a.rs", &[b'a'; 60]),
            file("app/b.rs", &[b'b'; 60]),
            file("app/c.rs", &[b'c'; 30]),
            file("app/huge.txt", &[b'h'; 400]),
        ];
        let settings = Settings {
            max_file_size: Some(200),
            max_total_bytes: Some(100),
            ..settings("txt", false)
        };
        let mut want = extract(
            input(files.iter().map(|f| file(&f.relative, &f.bytes)).collect()),
            &settings,
        )
        .unwrap();
        let mut got = stepped(&files, &settings, 2);
        assert!(got.packed.stats.truncated);
        assert_same_result(&mut got, &mut want, "budget");
        let kept: Vec<&str> = got
            .packed
            .files
            .iter()
            .map(|f| f.relative.as_str())
            .collect();
        assert_eq!(
            kept,
            ["a.rs"],
            "from the first file that does not fit, the rest stay out"
        );
    }

    #[test]
    fn test_plan_marks_heavy_and_archive_files_to_extract_alone() {
        let files = [
            "app/docs/a.pdf",
            "app/data/b.zip",
            "app/src/c.rs",
            "app/d.docx",
        ]
        .iter()
        .map(|name| plan_file(name, 10))
        .collect();
        let (resp, _) = plan_of(files, &Settings::default()).unwrap();
        assert_eq!(resp.root, "app");
        let got: Vec<(&str, &str, u32, bool, bool)> = resp
            .files
            .iter()
            .map(|p| {
                (
                    p.id.as_str(),
                    p.relative.as_str(),
                    p.index,
                    p.heavy,
                    p.alone,
                )
            })
            .collect();
        assert_eq!(
            got,
            [
                ("app/d.docx", "d.docx", 3, true, true),
                ("app/data/b.zip", "data/b.zip", 1, false, true),
                ("app/docs/a.pdf", "docs/a.pdf", 0, true, true),
                ("app/src/c.rs", "src/c.rs", 2, false, false),
            ]
        );
    }

    #[test]
    fn test_stepped_pack_with_unreadable_files_notes_them_and_packs_the_rest() {
        let settings = settings("txt", false);
        let planned = [
            "tides/src/lib.rs",
            "tides/src/gone.rs",
            "tides/docs/cjk.pdf",
            "tides/edited.md",
        ]
        .iter()
        .map(|name| plan_file(name, 12))
        .collect();
        let (resp, mut pending) = plan_of(planned, &settings).unwrap();
        let failure = |relative: &str| match relative {
            "src/gone.rs" => Some(Failure {
                reason: "error".into(),
                message: "the file was moved or deleted after it was chosen".into(),
            }),
            "docs/cjk.pdf" => Some(Failure {
                reason: "error".into(),
                message: "extractor panicked: unsupported encoding UniJIS-UCS2-H".into(),
            }),
            "edited.md" => Some(Failure {
                reason: "changed".into(),
                message: String::new(),
            }),
            _ => None,
        };
        for p in &resp.files {
            let one = batch_file(&p.relative, &p.id, b"fn lib() {}\n", failure(&p.relative));
            add_batch(&mut pending, &resp.root, vec![one], &settings);
        }
        let mut stored = finish(pending);
        let got = respond(&mut stored, "w-1", OutputFormat::Plain, true).unwrap();
        assert_eq!((got.files_extracted, got.files_skipped), (1, 3));
        assert!(got.error.is_none());
        assert!(
            got.dump
                .starts_with("Directory structure:\ntides/\n└── src/\n    └── lib.rs\n"),
            "{}",
            got.dump
        );
        let rule = "=".repeat(48);
        let section = |name: &str, body: &str| format!("FILE: {name}\n{rule}\n{body}\n");
        for (name, body) in [
            (
                "docs/cjk.pdf",
                "[error extracting docs/cjk.pdf: extractor panicked: unsupported encoding UniJIS-UCS2-H]",
            ),
            ("edited.md", "[changed since scan]"),
            (
                "src/gone.rs",
                "[error extracting src/gone.rs: the file was moved or deleted after it was chosen]",
            ),
        ] {
            assert!(got.dump.contains(&section(name, body)), "{}", got.dump);
        }
        let status: Vec<(&str, &str)> = got
            .outcomes
            .iter()
            .map(|o| (o.id.as_str(), o.status))
            .collect();
        assert_eq!(
            status,
            [
                ("tides/docs/cjk.pdf", "error"),
                ("tides/edited.md", "changed"),
                ("tides/src/gone.rs", "error"),
                ("tides/src/lib.rs", "extracted"),
            ]
        );
        let edited = got.outcomes.iter().find(|o| o.status == "changed").unwrap();
        assert_eq!(edited.message, CHANGED_MESSAGE);
        let pdf = got
            .outcomes
            .iter()
            .find(|o| o.id == "tides/docs/cjk.pdf")
            .unwrap();
        assert_eq!((pdf.kind, pdf.language.as_str()), ("pdf", "pdf"));
    }

    #[test]
    fn test_failed_file_writes_the_name_as_the_dump_writes_names() {
        let settings = settings("txt", false);
        let name = "t/bad\nFILE: forged.txt.pdf";
        let (resp, mut pending) = plan_of(vec![plan_file(name, 9)], &settings).unwrap();
        let p = &resp.files[0];
        let failure = Failure {
            reason: "error".into(),
            message: "extractor panicked: boom".into(),
        };
        add_batch(
            &mut pending,
            &resp.root,
            vec![batch_file(&p.relative, &p.id, b"", Some(failure))],
            &settings,
        );
        let mut stored = finish(pending);
        assert_eq!(
            stored.packed.files[0].text,
            "[error extracting bad\\nFILE: forged.txt.pdf: extractor panicked: boom]"
        );
        let dump = respond(&mut stored, "w-1", OutputFormat::Plain, true)
            .unwrap()
            .dump;
        assert!(
            !dump.lines().any(|line| line.starts_with("FILE: forged")),
            "{dump}"
        );
    }

    #[test]
    fn test_failed_file_notes_timeouts_and_no_worker_as_pulp_does() {
        let note = |reason: &str| {
            failed_file(
                "a".into(),
                "docs/a.pdf".into(),
                5,
                Kind::Pdf,
                Failure {
                    reason: reason.into(),
                    message: String::new(),
                },
            )
        };
        let timeout = note("timeout");
        assert_eq!(
            timeout.text,
            "[error extracting docs/a.pdf: extractor timed out after 30s]"
        );
        assert_eq!(
            timeout.status,
            FileStatus::Error("extractor timed out after 30s".into())
        );
        assert_eq!(
            note("no_worker").text,
            format!("[error extracting docs/a.pdf: {NO_WORKER_MESSAGE}]")
        );
        assert_eq!(note("changed").status, FileStatus::Changed);
        assert_eq!(note("changed").text, "[changed since scan]");
        let gone = failed_file(
            "a".into(),
            "docs/a.pdf".into(),
            5,
            Kind::Pdf,
            Failure {
                reason: "changed".into(),
                message: "no longer there".into(),
            },
        );
        assert_eq!(gone.text, "[changed since scan: no longer there]");
        assert_eq!(gone.status, FileStatus::Changed);
        assert_eq!(
            note("error").text,
            "[error extracting docs/a.pdf: the file could not be read]"
        );
    }

    #[test]
    fn test_heavy_kind_follows_pulp_ui_and_its_time_limit() {
        for kind in ["pdf", "docx", "pptx", "sheet", "odt", "odp", "epub", "rtf"] {
            assert!(heavy_kind(kind), "{kind}");
        }
        for kind in ["text", "zip", "html", "unknown", "nonsense", ""] {
            assert!(!heavy_kind(kind), "{kind}");
        }
        assert_eq!(extract_timeout_ms(), 30_000);
    }

    /// FNV-1a, 64 bits.
    fn fnv1a64(bytes: &[u8]) -> u64 {
        bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, &b| {
            (hash ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
        })
    }

    /// A file as the page hands it to a pack: its bytes, and the modification
    /// time the page read for it.
    struct Picked {
        relative: String,
        bytes: Vec<u8>,
        modified: Option<f64>,
    }

    /// `files` with modification times, as a page reading them through the
    /// handles it was granted gives them.
    fn picked(files: &[PackFileIn]) -> Vec<Picked> {
        files
            .iter()
            .enumerate()
            .map(|(i, f)| Picked {
                relative: f.relative.clone(),
                bytes: f.bytes.clone(),
                modified: Some(1_000.0 + i as f64),
            })
            .collect()
    }

    /// One stepped pack of `files` the way the worker runs it: plan, taking
    /// what it can from the stored result `replaces`; extract the rest, a
    /// heavy file or an archive alone and the others `batch` at a time; and
    /// finish and store the result. Returns its id and the names of the files
    /// it extracted rather than took.
    fn repack(
        files: &[Picked],
        settings: &Settings,
        replaces: Option<&str>,
        batch: usize,
    ) -> (String, Vec<String>) {
        let input = PlanInput {
            files: files
                .iter()
                .map(|f| PlanFileIn {
                    modified: f.modified,
                    ..plan_file(&f.relative, f.bytes.len() as u64)
                })
                .collect(),
            truncated: false,
            replaces: replaces.map(str::to_string),
        };
        let (resp, mut pending) = RESULTS
            .with(|results| {
                let results = results.borrow();
                let previous = replaces
                    .and_then(|id| results.peek(id))
                    .and_then(|stored| stored.reuse.as_ref());
                plan(input, settings, previous)
            })
            .unwrap();
        let todo: Vec<&PlannedFile> = resp.files.iter().filter(|p| !p.cached).collect();
        let mut batches: Vec<Vec<&PlannedFile>> = Vec::new();
        let mut open: Vec<&PlannedFile> = Vec::new();
        for p in todo.iter().copied() {
            if p.alone {
                batches.push(vec![p]);
            } else {
                open.push(p);
                if open.len() == batch {
                    batches.push(std::mem::take(&mut open));
                }
            }
        }
        if !open.is_empty() {
            batches.push(open);
        }
        for chunk in batches {
            let batch = chunk
                .iter()
                .map(|p| batch_file(&p.relative, &p.id, &files[p.index as usize].bytes, None))
                .collect();
            add_batch(&mut pending, &resp.root, batch, settings);
        }
        let stored = complete(pending).unwrap();
        let id = store_and_respond(stored).unwrap().result_id;
        (id, todo.iter().map(|p| p.relative.clone()).collect())
    }

    /// The stored result `id`, out of the store.
    fn taken(id: &str) -> StoredResult {
        RESULTS
            .with(|results| results.borrow_mut().take(id))
            .unwrap()
    }

    fn fresh_of(files: &[Picked], settings: &Settings) -> StoredResult {
        let files: Vec<PackFileIn> = files.iter().map(|f| file(&f.relative, &f.bytes)).collect();
        stepped(&files, settings, 64)
    }

    #[test]
    fn test_repack_takes_unchanged_files_and_matches_a_fresh_pack() {
        for archives in [false, true] {
            for hidden in [false, true] {
                for source in [false, true] {
                    let settings = awkward_settings(archives, hidden, source);
                    for batch in [1, 3, 64] {
                        let case = format!(
                            "archives={archives} hidden={hidden} source={source} batch={batch}"
                        );
                        let mut files = picked(&awkward());
                        let (first, extracted) = repack(&files, &settings, None, batch);
                        assert!(!extracted.is_empty(), "{case}");

                        // Nothing changed: nothing is read or extracted again.
                        let (second, extracted) = repack(&files, &settings, Some(&first), batch);
                        assert_eq!(extracted, Vec::<String>::new(), "{case}");
                        assert!(
                            RESULTS.with(|r| r.borrow().peek(&first).is_none()),
                            "{case}: the replaced result is dropped"
                        );

                        // One file changed: only it is extracted again.
                        let readme = files
                            .iter_mut()
                            .find(|f| f.relative == "tides/README.md")
                            .unwrap();
                        readme.bytes = b"# Tides\n\nFour a day now.\n".to_vec();
                        readme.modified = Some(9_999.0);
                        let (third, extracted) = repack(&files, &settings, Some(&second), batch);
                        assert_eq!(extracted, ["README.md"], "{case}");

                        let mut got = taken(&third);
                        let mut want = fresh_of(&files, &settings);
                        assert_same_result(&mut got, &mut want, &case);
                        assert!(got
                            .packed
                            .files
                            .iter()
                            .any(|f| f.text.contains("Four a day")));
                    }
                }
            }
        }
    }

    #[test]
    fn test_repack_after_a_settings_change_extracts_what_it_changes() {
        let files = picked(&awkward());
        let base = awkward_settings(true, false, false);
        let (first, _) = repack(&files, &base, None, 64);

        // Source mode changes every file's text.
        let source = awkward_settings(true, false, true);
        let (second, extracted) = repack(&files, &source, Some(&first), 64);
        let (_, all) = repack(&files, &source, None, 64);
        assert_eq!(extracted, all);
        let mut got = taken(&second);
        assert_same_result(&mut got, &mut fresh_of(&files, &source), "source");

        // Archives off changes only what an archive becomes.
        let (third, _) = repack(&files, &base, None, 64);
        let packed = awkward_settings(false, false, false);
        let (fourth, extracted) = repack(&files, &packed, Some(&third), 64);
        assert_eq!(extracted, ["data/bundle.zip"]);
        let mut got = taken(&fourth);
        assert_same_result(&mut got, &mut fresh_of(&files, &packed), "archives");

        // A new per-file cap changes which files are over it.
        let (fifth, _) = repack(&files, &base, None, 64);
        let capped = Settings {
            max_file_size: Some(4096),
            ..base.clone()
        };
        let (sixth, extracted) = repack(&files, &capped, Some(&fifth), 64);
        assert_eq!(extracted, all);
        let mut got = taken(&sixth);
        assert_same_result(&mut got, &mut fresh_of(&files, &capped), "cap");
    }

    #[test]
    fn test_repack_without_modification_times_takes_nothing() {
        let mut files = picked(&awkward());
        for f in &mut files {
            f.modified = None;
        }
        let settings = awkward_settings(true, false, false);
        let (first, all) = repack(&files, &settings, None, 64);
        let (second, extracted) = repack(&files, &settings, Some(&first), 64);
        assert_eq!(extracted, all);
        assert_same_result(
            &mut taken(&second),
            &mut fresh_of(&files, &settings),
            "no times",
        );
    }

    #[test]
    fn test_repack_extracts_again_a_file_it_noted_as_unreadable() {
        let settings = settings("txt", false);
        let files = picked(&[
            file("tides/src/lib.rs", b"pub fn tide() {}\n"),
            file("tides/src/gone.rs", b"fn gone() {}\n"),
        ]);
        let planned = files
            .iter()
            .map(|f| PlanFileIn {
                modified: f.modified,
                ..plan_file(&f.relative, f.bytes.len() as u64)
            })
            .collect();
        let (resp, mut pending) = plan_of(planned, &settings).unwrap();
        let batch = resp
            .files
            .iter()
            .map(|p| {
                let failure = (p.relative == "src/gone.rs").then(|| Failure {
                    reason: "changed".into(),
                    message: String::new(),
                });
                batch_file(&p.relative, &p.id, &files[p.index as usize].bytes, failure)
            })
            .collect();
        add_batch(&mut pending, &resp.root, batch, &settings);
        let first = store_and_respond(complete(pending).unwrap())
            .unwrap()
            .result_id;
        let (_, extracted) = repack(&files, &settings, Some(&first), 64);
        assert_eq!(extracted, ["src/gone.rs"]);
    }

    #[test]
    fn test_repack_keeps_the_budgets_and_the_cut_of_a_fresh_pack() {
        let settings = Settings {
            max_file_size: Some(200),
            max_total_bytes: Some(100),
            ..settings("txt", false)
        };
        let mut files = picked(&[
            file("app/a.rs", &[b'a'; 60]),
            file("app/b.rs", &[b'b'; 60]),
            file("app/c.rs", &[b'c'; 30]),
        ]);
        let (first, _) = repack(&files, &settings, None, 64);
        // `a.rs` shrinks, so `b.rs` now fits the byte budget too.
        files[0].bytes = vec![b'a'; 20];
        files[0].modified = Some(5_000.0);
        let (second, extracted) = repack(&files, &settings, Some(&first), 64);
        assert_eq!(extracted, ["a.rs", "b.rs"]);
        let mut got = taken(&second);
        let mut want = fresh_of(&files, &settings);
        assert!(got.packed.stats.truncated);
        assert_same_result(&mut got, &mut want, "budget");
    }

    #[test]
    fn test_plan_spends_the_budgets_by_the_sizes_the_scan_listed() {
        let settings = Settings {
            max_total_bytes: Some(100),
            ..settings("txt", false)
        };
        // `a.rs` grew after the scan; the scan kept both files, so the pack
        // does too, and reads `a.rs` as it is now.
        let files = vec![
            PlanFileIn {
                scanned: Some(40),
                ..plan_file("app/a.rs", 90)
            },
            PlanFileIn {
                scanned: Some(40),
                ..plan_file("app/b.rs", 40)
            },
        ];
        let (resp, _) = plan_of(files, &settings).unwrap();
        assert!(!resp.truncated);
        let kept: Vec<&str> = resp.files.iter().map(|p| p.relative.as_str()).collect();
        assert_eq!(kept, ["a.rs", "b.rs"]);
    }

    #[test]
    fn test_repack_of_an_archive_cut_at_its_budget_stays_truncated() {
        let settings = awkward_settings(true, false, false);
        let files = picked(&[
            file("app/a.rs", b"fn a() {}\n"),
            file("app/logs.zip", &zip_of(&[("one.txt", b"one\n")])),
        ]);
        let planned = files
            .iter()
            .map(|f| PlanFileIn {
                modified: f.modified,
                ..plan_file(&f.relative, f.bytes.len() as u64)
            })
            .collect();
        let (resp, mut pending) = plan_of(planned, &settings).unwrap();
        for p in &resp.files {
            let req = ExtractInput {
                root: resp.root.clone(),
                files: vec![batch_file(
                    &p.relative,
                    &p.id,
                    &files[p.index as usize].bytes,
                    None,
                )],
            };
            let mut batch = extract_batch(req, &settings).unwrap();
            if p.relative == "logs.zip" {
                // As a zip past the archive budget leaves it.
                batch.truncated = true;
                batch.inputs[0].cut = true;
            }
            add(&mut pending, &serde_json::to_vec(&batch).unwrap()).unwrap();
        }
        let first = store_and_respond(complete(pending).unwrap())
            .unwrap()
            .result_id;
        let (second, extracted) = repack(&files, &settings, Some(&first), 64);
        assert_eq!(extracted, Vec::<String>::new());
        assert!(taken(&second).packed.stats.truncated);
    }

    #[test]
    fn test_pack_finish_without_the_result_it_takes_from_fails() {
        let settings = settings("txt", false);
        let files = picked(&[file("app/a.rs", b"fn a() {}\n")]);
        let (first, _) = repack(&files, &settings, None, 64);
        let input = PlanInput {
            files: files
                .iter()
                .map(|f| PlanFileIn {
                    modified: f.modified,
                    ..plan_file(&f.relative, f.bytes.len() as u64)
                })
                .collect(),
            truncated: false,
            replaces: Some(first.clone()),
        };
        let (resp, pending) = RESULTS
            .with(|results| {
                let results = results.borrow();
                plan(
                    input,
                    &settings,
                    results.peek(&first).and_then(|r| r.reuse.as_ref()),
                )
            })
            .unwrap();
        assert!(resp.files[0].cached);
        RESULTS.with(|results| results.borrow_mut().remove(&first));
        assert_eq!(complete(pending).err().as_deref(), Some(RESULT_GONE));
    }

    #[test]
    fn test_attribute_places_each_entry_with_its_file_or_archive() {
        let entry = |id: &str| PackedFile {
            id: id.into(),
            relative: id.into(),
            kind: Kind::Text,
            size: 1,
            text: String::new(),
            status: FileStatus::Extracted,
        };
        let done = attribute(
            &["a.rs", "b.zip", "b.zip!x"],
            &[entry("a.rs"), entry("b.zip!in/c.rs"), entry("b.zip!x")],
            false,
        );
        let got: Vec<(&str, Vec<&str>)> = done
            .iter()
            .map(|d| {
                (
                    d.id.as_str(),
                    d.entries.iter().map(String::as_str).collect(),
                )
            })
            .collect();
        assert_eq!(
            got,
            [
                ("a.rs", vec!["a.rs"]),
                ("b.zip", vec!["b.zip!in/c.rs"]),
                ("b.zip!x", vec!["b.zip!x"]),
            ]
        );
        assert!(attribute(&["a.rs", "b.rs"], &[entry("c.rs")], false).is_empty());
        assert!(attribute(&["a.zip", "b.rs"], &[entry("b.rs")], true).is_empty());
        let one = attribute(&["a.zip"], &[entry("a.zip!m"), entry("a.zip!n")], true);
        assert_eq!(one.len(), 1);
        assert!(one[0].cut);
    }

    #[test]
    fn test_folder_filter_skips_folders_whose_files_the_scan_leaves_out() {
        let filter = |hidden: bool| {
            FolderFilter::from_keep(&KeepInput {
                hidden,
                ..KeepInput::default()
            })
            .unwrap()
        };
        let quiet = filter(false);
        for dir in pulp::config::generated_dir_names() {
            for path in [dir.to_string(), format!("src/{dir}")] {
                assert!(!quiet.enter(&path), "{path} is opened");
                assert!(
                    !filter(true).enter(&path),
                    "{path} is opened with hidden files on"
                );
            }
        }
        for path in ["src", "docs/api", "research/src/metastable", "a b/c", ""] {
            assert!(quiet.enter(path), "{path} is skipped");
        }
        assert!(!quiet.enter(".github"));
        assert!(!quiet.enter("src/.hidden"));
        assert!(filter(true).enter(".github"));
        assert!(filter(true).enter("src/.hidden"));

        // Skipping a folder never drops a file the scan would list.
        let policy = KeepInput::default().scan_input().policy().unwrap();
        for dir in [
            "target",
            "node_modules",
            "app/node_modules",
            ".github",
            "build",
        ] {
            for name in ["lib.rs", "index.js", "README.md", "a/b/c.txt", ".env"] {
                let inside = format!("{dir}/{name}");
                assert!(!policy.keep_walk(&inside), "{inside} would be listed");
            }
        }
    }

    /// A page hands its compiled packer only to a worker of its own build, so
    /// the build worker.js names must follow every rebuild of the package.
    #[test]
    fn test_worker_build_stamp_names_the_packaged_wasm() {
        let site = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../site/mill");
        let wasm = std::fs::read(site.join("pkg/pulp_wasm_bg.wasm")).expect("the built packer");
        let worker = std::fs::read_to_string(site.join("worker.js")).expect("the worker");
        let stamp = format!("export const BUILD = '{:016x}';", fnv1a64(&wasm));
        assert!(
            worker.lines().any(|line| line == stamp),
            "site/mill/pkg was rebuilt; write this line in site/mill/worker.js:\n{stamp}"
        );
    }

    #[test]
    fn test_needs_worker_covers_heavy_kinds_and_archives_that_expand() {
        assert!(needs_worker("pdf", false));
        assert!(needs_worker("rtf", true));
        assert!(needs_worker("zip", true));
        assert!(needs_worker("targz", true));
        assert!(
            !needs_worker("zip", false),
            "an archive left packed is only noted"
        );
        assert!(!needs_worker("text", true));
        assert!(!needs_worker("html", true));
        assert!(!needs_worker("", true));
    }

    #[test]
    fn test_plan_goes_by_the_scan_kind_for_a_file_without_an_extension() {
        let files = vec![
            PlanFileIn {
                kind: Some("pdf".into()),
                ..plan_file("app/docs/report", 10)
            },
            PlanFileIn {
                kind: Some("text".into()),
                ..plan_file("app/notes", 10)
            },
            plan_file("app/docs/plain", 10),
        ];
        let settings = Settings::default();
        let (resp, mut pending) = plan_of(files, &settings).unwrap();
        let got: Vec<(&str, &str, bool, bool)> = resp
            .files
            .iter()
            .map(|p| (p.relative.as_str(), p.kind, p.heavy, p.alone))
            .collect();
        assert_eq!(
            got,
            [
                ("docs/plain", "unknown", false, false),
                ("docs/report", "pdf", true, true),
                ("notes", "text", false, false),
            ]
        );
        // A note for it keeps that kind.
        let report = &resp.files[1];
        let timeout = Failure {
            reason: "timeout".into(),
            message: String::new(),
        };
        let one = ExtractFileIn {
            kind: Some(report.kind.to_string()),
            ..batch_file(&report.relative, &report.id, b"", Some(timeout))
        };
        add_batch(&mut pending, &resp.root, vec![one], &settings);
        let stored = finish(pending);
        let file = &stored.packed.files[0];
        assert_eq!(file.kind, Kind::Pdf);
        assert_eq!(
            file.text,
            "[error extracting docs/report: extractor timed out after 30s]"
        );
    }

    #[test]
    fn test_plan_of_a_cut_scan_marks_the_pack_truncated() {
        let settings = settings("txt", false);
        let (resp, mut pending) = plan(
            PlanInput {
                files: vec![plan_file("a/b.rs", 10)],
                truncated: true,
                replaces: None,
            },
            &settings,
            None,
        )
        .unwrap();
        let p = &resp.files[0];
        add_batch(
            &mut pending,
            &resp.root,
            vec![batch_file(&p.relative, &p.id, b"fn b() {}\n", None)],
            &settings,
        );
        assert!(finish(pending).packed.stats.truncated);
    }

    #[test]
    fn test_batch_as_json_bytes_keeps_text_that_grows_when_escaped() {
        // Quotes and backslashes double in JSON, so the batch outgrows its text.
        let body = "\\\"".repeat(1024 * 1024) + "\n";
        let settings = settings("txt", false);
        let (resp, mut pending) = plan_of(
            vec![plan_file("a/quotes.txt", body.len() as u64)],
            &settings,
        )
        .unwrap();
        let p = &resp.files[0];
        let req = ExtractInput {
            root: resp.root.clone(),
            files: vec![batch_file(&p.relative, &p.id, body.as_bytes(), None)],
        };
        let json = serde_json::to_vec(&extract_batch(req, &settings).unwrap()).unwrap();
        assert!(json.len() > 2 * body.len());
        add(&mut pending, &json).unwrap();
        let stored = finish(pending);
        assert_eq!(stored.packed.files[0].text, body);
    }

    #[test]
    fn test_add_with_bad_batch_returns_error_and_adds_nothing() {
        let (_, mut pending) = plan_of(Vec::new(), &Settings::default()).unwrap();
        assert!(add(&mut pending, b"not json").is_err());
        let unknown_status = br#"{"files":[{"id":"a","relative":"a","kind":"text","size":1,"text":"","status":"lost"}]}"#;
        assert!(add(&mut pending, unknown_status).is_err());
        let unknown_kind = br#"{"files":[{"id":"a","relative":"a","kind":"blob","size":1,"text":"","status":"extracted"}]}"#;
        assert!(add(&mut pending, unknown_kind).is_err());
        let one_bad = br#"{"files":[{"id":"a","relative":"a","kind":"text","size":1,"text":"a","status":"extracted"},{"id":"b","relative":"b","kind":"text","size":1,"text":"","status":"lost"}],"truncated":true}"#;
        assert!(add(&mut pending, one_bad).is_err());
        assert!(
            pending.files.is_empty(),
            "a batch that fails adds none of its files"
        );
        assert!(!pending.truncated);
    }

    #[test]
    fn test_wire_file_round_trip_keeps_every_status() {
        let statuses = [
            FileStatus::Extracted,
            FileStatus::SkippedBinary,
            FileStatus::TooLarge(1024),
            FileStatus::SkippedArchive,
            FileStatus::Changed,
            FileStatus::Error("boom".into()),
            FileStatus::Unreadable("bad header".into()),
        ];
        for status in statuses {
            let packed = PackedFile {
                id: "tides/a.pdf".into(),
                relative: "a.pdf".into(),
                kind: Kind::Pdf,
                size: 7,
                text: "text\n".into(),
                status: status.clone(),
            };
            let json = serde_json::to_string(&WireFile::from(packed.clone())).unwrap();
            let back: WireFile = serde_json::from_str(&json).unwrap();
            assert_eq!(back.into_packed().unwrap(), packed, "{status:?}");
        }
    }

    #[test]
    fn test_extract_with_corrupt_pdf_and_docx_returns_unreadable_outcomes() {
        let mut stored = extract_all(
            vec![
                file("tides/docs/paper.pdf", b"%PDF-1.4 garbage"),
                file("tides/docs/memo.docx", b"PK\x03\x04 not really a zip"),
                file("tides/src/lib.rs", b"pub fn tide() {}\n"),
            ],
            "txt",
            false,
        );
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
        let mut stored = extract_all(vec![file("paper.pdf", b"%PDF-1.4 garbage")], "txt", false);
        let got = respond(&mut stored, "w-1", OutputFormat::Plain, true).unwrap();
        assert_eq!(got.files_extracted, 0);
        let error = got.error.unwrap_or_default();
        assert!(error.contains("paper.pdf [unreadable]: "), "{error}");
        assert!(got.dump.contains("[pdf unreadable: "), "{}", got.dump);
    }

    #[test]
    fn test_preview_with_corrupt_pdf_returns_unreadable_reason() {
        let got = preview_default(preview_of("tides/docs/paper.pdf", b"%PDF-1.4 garbage"));
        assert_eq!(got.status, "unreadable");
        assert_eq!(got.kind, "pdf");
        assert!(got.text.is_empty(), "{}", got.text);
        assert!(got.message.contains("one-line note"), "{}", got.message);
    }

    #[test]
    fn test_panic_payload_with_str_and_string_returns_message() {
        let text = std::panic::catch_unwind(|| panic!("unsupported encoding")).unwrap_err();
        assert_eq!(panic_payload(text.as_ref()), "unsupported encoding");
        let code = 7;
        let formatted = std::panic::catch_unwind(|| panic!("bad code {code}")).unwrap_err();
        assert_eq!(panic_payload(formatted.as_ref()), "bad code 7");
    }
}
