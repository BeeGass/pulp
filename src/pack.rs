#[cfg(feature = "native")]
use std::io::Read;
use std::path::{Component, Path, PathBuf};
#[cfg(feature = "native")]
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

#[cfg(feature = "native")]
use rayon::prelude::*;

use crate::classify::{Kind, classify, looks_binary};
use crate::config::{Options, TreeMode, apply_budgets, cmp_path_order};
use crate::error::Error;
use crate::extract::isolate::panic_message;
use crate::extract::{ExtractOpts, expand_archive, extract};
#[cfg(feature = "native")]
use crate::manifest::ManifestEntry;
use crate::tree::display_path;

const MAX_ARCHIVE_DEPTH: u8 = 3;
const MAX_ARCHIVE_UNCOMPRESSED: u64 = 512 * 1024 * 1024;

/// Outcome of packing one input (or archive member).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileStatus {
    Extracted,
    SkippedBinary,
    /// Per-file cap, in bytes, that this file exceeded.
    TooLarge(u64),
    SkippedArchive,
    Changed,
    Error(String),
    /// The parser rejected the file, so the dump holds a one-line note in its
    /// place. Holds the parser's message.
    Unreadable(String),
}

/// One file (or archive member) in a [`Packed`] dump.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackedFile {
    pub id: String,
    pub relative: String,
    pub kind: Kind,
    pub size: u64,
    pub text: String,
    pub status: FileStatus,
}

impl FileStatus {
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Extracted => "extracted",
            Self::SkippedBinary => "skipped_binary",
            Self::TooLarge(_) => "too_large",
            Self::SkippedArchive => "skipped_archive",
            Self::Changed => "changed",
            Self::Error(_) => "error",
            Self::Unreadable(_) => "unreadable",
        }
    }

    #[must_use]
    pub fn message(&self, size: u64) -> String {
        match self {
            Self::Extracted => String::new(),
            Self::SkippedBinary => format!(
                "Skipped a binary file ({size} bytes). The bytes are not written into the dump. Enable binaries to keep a one-line placeholder instead."
            ),
            Self::TooLarge(limit) => format!(
                "File is {size} bytes, over the {limit}-byte cap. Raise the cap or leave this file unchecked."
            ),
            Self::SkippedArchive => format!(
                "Archive ({size} bytes) was not expanded. Turn on archives to unpack zip and tar members into the dump."
            ),
            Self::Changed => "File changed on disk after the scan. Rescan, then pulp again so the dump matches the current bytes.".into(),
            Self::Error(err) => format!("Extraction failed for a {size}-byte file. {err}"),
            Self::Unreadable(reason) => format!(
                "Could not parse this {size}-byte file, so the dump holds a one-line note in its place. It may be damaged, encrypted, or too large to unpack. {reason}"
            ),
        }
    }
}

/// Totals for a pack run.
#[derive(Debug, Clone)]
pub struct Stats {
    /// Files whose text went into the dump ([`FileStatus::Extracted`]).
    pub files_extracted: usize,
    /// Every other file, unreadable ones included. The dump holds a one-line
    /// note for each.
    pub files_skipped: usize,
    pub bytes_read: u64,
    pub chars_emitted: usize,
    pub tokens_est: usize,
    pub elapsed: Duration,
    pub truncated: bool,
    pub cancelled: bool,
}

impl Default for Stats {
    fn default() -> Self {
        Self {
            files_extracted: 0,
            files_skipped: 0,
            bytes_read: 0,
            chars_emitted: 0,
            tokens_est: 0,
            elapsed: Duration::ZERO,
            truncated: false,
            cancelled: false,
        }
    }
}

/// Walked, extracted files plus an optional directory map.
#[derive(Debug, Clone)]
pub struct Packed {
    pub files: Vec<PackedFile>,
    pub tree: String,
    pub stats: Stats,
}

/// One in-memory input for [`pack_entries`].
#[derive(Debug, Clone, Copy)]
pub struct MemoryFile<'a> {
    pub id: &'a str,
    pub relative: &'a str,
    pub bytes: &'a [u8],
}

struct WorkItem {
    id: String,
    relative: String,
    absolute: Option<PathBuf>,
    bytes: Vec<u8>,
    depth: u8,
}

fn pack_clock_start() -> Option<Instant> {
    #[cfg(target_arch = "wasm32")]
    {
        None
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        Some(Instant::now())
    }
}

fn pack_clock_elapsed(start: Option<Instant>) -> Duration {
    match start {
        Some(t) => t.elapsed(),
        None => Duration::ZERO,
    }
}

/// Walk `opts.roots` and extract each file into LLM-readable text.
#[cfg(feature = "native")]
pub fn pack(opts: &Options) -> Result<Packed, Error> {
    pack_with_cancel(opts, None)
}

/// [`pack`] with cooperative cancel checked between files.
#[cfg(feature = "native")]
pub fn pack_with_cancel(opts: &Options, cancel: Option<&AtomicBool>) -> Result<Packed, Error> {
    let start = pack_clock_start();
    if opts.selection.is_empty_only() {
        return Ok(Packed {
            files: Vec::new(),
            tree: String::new(),
            stats: Stats {
                elapsed: pack_clock_elapsed(start),
                ..Stats::default()
            },
        });
    }
    let manifest = crate::manifest::scan_manifest(opts)?;
    pack_manifest(&manifest, opts, cancel, None, start)
}

/// Extract already-discovered entries. Used by the mill to avoid a second walk.
///
/// `progress` counts entries as they finish, so another thread can report how
/// far the pack has got.
#[cfg(feature = "native")]
pub fn pack_manifest(
    manifest: &crate::manifest::ScanManifest,
    opts: &Options,
    cancel: Option<&AtomicBool>,
    progress: Option<&AtomicUsize>,
    start: Option<Instant>,
) -> Result<Packed, Error> {
    let extract_opts = ExtractOpts::from_options(opts);
    let bytes_read = AtomicU64::new(0);

    let mut files = run_parallel(opts.jobs, || {
        manifest
            .entries
            .par_iter()
            .flat_map(|entry| {
                let files = process_entry(entry, opts, &extract_opts, &bytes_read, cancel);
                if let Some(done) = progress {
                    done.fetch_add(1, Ordering::SeqCst);
                }
                files
            })
            .collect::<Vec<PackedFile>>()
    })?;
    files.sort_by(|a, b| a.relative.cmp(&b.relative));
    let cancelled = cancel.is_some_and(|c| c.load(Ordering::Relaxed));

    let tree = render_pack_tree(opts, &files);
    let files_extracted = files
        .iter()
        .filter(|f| f.status == FileStatus::Extracted)
        .count();
    let files_skipped = files.len().saturating_sub(files_extracted);

    let chunks = std::iter::once(tree.as_str()).chain(
        files
            .iter()
            .filter(|file| file.status == FileStatus::Extracted)
            .map(|file| file.text.as_str()),
    );
    let (chars_emitted, tokens_est) = crate::tokens::summarize_chunks(chunks);

    Ok(Packed {
        files,
        tree,
        stats: Stats {
            files_extracted,
            files_skipped,
            bytes_read: bytes_read.load(Ordering::Relaxed),
            chars_emitted,
            tokens_est,
            elapsed: pack_clock_elapsed(start),
            truncated: manifest.truncated || cancelled,
            cancelled,
        },
    })
}

/// Extract one manifest entry on the calling thread, as [`pack_manifest`]
/// would. Archive members come back sorted by relative path.
///
/// The mill preview uses this so it never queues behind a pack on the shared
/// Rayon pool.
#[cfg(feature = "native")]
pub(crate) fn pack_manifest_entry(entry: &ManifestEntry, opts: &Options) -> Vec<PackedFile> {
    let extract_opts = ExtractOpts::from_options(opts);
    let mut files = process_entry(entry, opts, &extract_opts, &AtomicU64::new(0), None);
    files.sort_by(|a, b| a.relative.cmp(&b.relative));
    files
}

/// Pack an in-memory file set (browser / virtual trees). No filesystem reads.
///
/// Enforces [`Options::selection`], include/exclude, hidden, and the entry
/// and byte budgets. The budgets are spent in path order whatever order
/// `entries` come in, so they keep the files a walk of the same folder keeps
/// (see [`crate::config::Budget`]).
pub fn pack_entries(
    entries: &[MemoryFile<'_>],
    opts: &Options,
    cancel: Option<&AtomicBool>,
) -> Result<Packed, Error> {
    let start = pack_clock_start();
    if opts.selection.is_empty_only() {
        return Ok(Packed {
            files: Vec::new(),
            tree: String::new(),
            stats: Stats {
                elapsed: pack_clock_elapsed(start),
                ..Stats::default()
            },
        });
    }
    let policy = crate::filter::PathPolicy::from_options(opts)?;
    let extract_opts = ExtractOpts::from_options(opts);
    let bytes_read = AtomicU64::new(0);
    let mut chosen: Vec<&MemoryFile<'_>> = entries
        .iter()
        .filter(|entry| {
            (opts.selection.allows(entry.id) || opts.selection.allows(entry.relative))
                && policy.keep_walk(entry.relative)
        })
        .collect();
    chosen.sort_by(|a, b| cmp_path_order(a.relative, b.relative).then_with(|| a.id.cmp(b.id)));
    let truncated = apply_budgets(&mut chosen, opts, |entry| entry.bytes.len() as u64);
    let mut files: Vec<PackedFile> = Vec::new();
    for entry in chosen {
        if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
            break;
        }
        let size = entry.bytes.len() as u64;
        if size > opts.max_file_size {
            files.push(packed_too_large(
                entry.id.to_string(),
                entry.relative.to_string(),
                classify(Path::new(entry.relative), Some(entry.bytes)),
                size,
                opts.max_file_size,
            ));
            continue;
        }
        bytes_read.fetch_add(size, Ordering::Relaxed);
        files.extend(process_item(
            WorkItem {
                id: entry.id.to_string(),
                relative: entry.relative.to_string(),
                absolute: None,
                bytes: entry.bytes.to_vec(),
                depth: 0,
            },
            opts,
            &extract_opts,
        ));
    }
    files.sort_by(|a, b| a.relative.cmp(&b.relative));
    let cancelled = cancel.is_some_and(|c| c.load(Ordering::Relaxed));
    let tree = render_pack_tree(opts, &files);
    let files_extracted = files
        .iter()
        .filter(|f| f.status == FileStatus::Extracted)
        .count();
    let files_skipped = files.len().saturating_sub(files_extracted);
    let chunks = std::iter::once(tree.as_str()).chain(
        files
            .iter()
            .filter(|file| file.status == FileStatus::Extracted)
            .map(|file| file.text.as_str()),
    );
    let (chars_emitted, tokens_est) = crate::tokens::summarize_chunks(chunks);
    Ok(Packed {
        files,
        tree,
        stats: Stats {
            files_extracted,
            files_skipped,
            bytes_read: bytes_read.load(Ordering::Relaxed),
            chars_emitted,
            tokens_est,
            elapsed: pack_clock_elapsed(start),
            truncated: truncated || cancelled,
            cancelled,
        },
    })
}

#[cfg(feature = "native")]
fn run_parallel<T, F>(jobs: usize, f: F) -> Result<T, Error>
where
    T: Send,
    F: FnOnce() -> T + Send,
{
    // wasm32 has no threads; building a Rayon pool panics as `unreachable`.
    #[cfg(target_arch = "wasm32")]
    {
        let _ = jobs;
        return Ok(f());
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        if jobs == 0 {
            Ok(f())
        } else {
            rayon::ThreadPoolBuilder::new()
                .num_threads(jobs)
                .build()
                .map_err(|err| Error::msg(err.to_string()))
                .map(|pool| pool.install(f))
        }
    }
}

#[cfg(feature = "native")]
fn process_entry(
    entry: &ManifestEntry,
    opts: &Options,
    extract_opts: &ExtractOpts,
    bytes_read: &AtomicU64,
    cancel: Option<&AtomicBool>,
) -> Vec<PackedFile> {
    if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
        return Vec::new();
    }
    if opts.list_only {
        return vec![list_only_file(entry, opts)];
    }
    if let Some(msg) = changed_since_scan(entry) {
        return vec![PackedFile {
            id: entry.id.clone(),
            relative: entry.relative.clone(),
            kind: entry.kind,
            size: entry.size,
            text: format!("[{msg}]"),
            status: FileStatus::Changed,
        }];
    }
    if entry.size > opts.max_file_size {
        return vec![packed_too_large(
            entry.id.clone(),
            entry.relative.clone(),
            entry.kind,
            entry.size,
            opts.max_file_size,
        )];
    }
    let bytes = match read_limited(&entry.absolute, opts.max_file_size) {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(size)) => {
            return vec![packed_too_large(
                entry.id.clone(),
                entry.relative.clone(),
                entry.kind,
                size,
                opts.max_file_size,
            )];
        }
        Err(err) => {
            return vec![packed_error(
                entry.id.clone(),
                entry.relative.clone(),
                entry.kind,
                entry.size,
                err.to_string(),
            )];
        }
    };
    bytes_read.fetch_add(bytes.len() as u64, Ordering::Relaxed);
    process_item(
        WorkItem {
            id: entry.id.clone(),
            relative: entry.relative.clone(),
            absolute: Some(entry.absolute.clone()),
            bytes,
            depth: 0,
        },
        opts,
        extract_opts,
    )
}

#[cfg(feature = "native")]
fn read_limited(path: &Path, max: u64) -> std::io::Result<Result<Vec<u8>, u64>> {
    let file = std::fs::File::open(path)?;
    let mut buf = Vec::new();
    let n = file.take(max.saturating_add(1)).read_to_end(&mut buf)?;
    if n as u64 > max {
        let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(n as u64);
        Ok(Err(size))
    } else {
        Ok(Ok(buf))
    }
}

fn process_item(item: WorkItem, opts: &Options, extract_opts: &ExtractOpts) -> Vec<PackedFile> {
    let kind = classify(class_path(&item), Some(&item.bytes));
    let size = item.bytes.len() as u64;
    let should_expand = kind.is_archive()
        && item.depth < MAX_ARCHIVE_DEPTH
        && size <= opts.max_file_size
        && (opts.follow_archives
            || item
                .absolute
                .as_ref()
                .is_some_and(|path| is_input_root(path, &opts.roots)));
    if should_expand {
        return expand_item(
            &item.id,
            &item.relative,
            &item.bytes,
            kind,
            opts,
            extract_opts,
            item.depth,
        );
    }
    vec![pack_one(
        item.id,
        item.relative,
        item.absolute.as_deref(),
        kind,
        size,
        &item.bytes,
        opts,
        extract_opts,
    )]
}

#[cfg(feature = "native")]
fn changed_since_scan(entry: &ManifestEntry) -> Option<String> {
    let meta = std::fs::metadata(&entry.absolute).ok()?;
    if meta.len() != entry.size {
        return Some(format!(
            "changed since scan: size {} -> {}",
            entry.size,
            meta.len()
        ));
    }
    if let (Some(was), Ok(now)) = (entry.modified, meta.modified()) {
        if was != now {
            return Some("changed since scan".into());
        }
    }
    None
}

#[allow(clippy::too_many_arguments)]
fn pack_one(
    id: String,
    relative: String,
    path: Option<&Path>,
    kind: Kind,
    size: u64,
    bytes: &[u8],
    opts: &Options,
    extract_opts: &ExtractOpts,
) -> PackedFile {
    if size > opts.max_file_size {
        return packed_too_large(id, relative, kind, size, opts.max_file_size);
    }
    // A name says text but the bytes are binary: an MPEG-TS `.ts`, a binary
    // `.dat`. Treat it as the binary it is rather than dump it as mojibake.
    let kind = if is_text_like(kind) && looks_binary(bytes) {
        Kind::Binary
    } else {
        kind
    };
    if should_skip_binary(kind, bytes, opts.skip_binaries) {
        return PackedFile {
            text: format!("[binary file, {size} bytes]"),
            id,
            relative,
            kind,
            size,
            status: FileStatus::SkippedBinary,
        };
    }
    if kind.is_archive() {
        // Names in a note are escaped as in a header, so one cannot end the
        // note's line and forge a section of its own.
        let name = display_path(&relative);
        return PackedFile {
            text: format!(
                "[archive {name}, {size} bytes; pass --archives to expand nested archives]"
            ),
            id,
            relative,
            kind,
            size,
            status: FileStatus::SkippedArchive,
        };
    }
    let extracted = extract_contained(path, bytes, kind, &relative, extract_opts);
    match extracted {
        Ok(text) => PackedFile {
            id,
            relative,
            kind,
            size,
            text,
            status: FileStatus::Extracted,
        },
        Err(Error::Unreadable(reason)) => packed_unreadable(id, relative, kind, size, reason),
        Err(err) => packed_error(id, relative, kind, size, err.to_string()),
    }
}

fn expand_item(
    id: &str,
    relative: &str,
    bytes: &[u8],
    kind: Kind,
    opts: &Options,
    extract_opts: &ExtractOpts,
    depth: u8,
) -> Vec<PackedFile> {
    match expand_archive(bytes, kind, extract_opts) {
        Ok(members) => take_archive_members(id, relative, members, opts, extract_opts, depth),
        Err(err) => vec![packed_error(
            id.to_string(),
            relative.to_string(),
            kind,
            bytes.len() as u64,
            err.to_string(),
        )],
    }
}

fn take_archive_members(
    parent_id: &str,
    relative: &str,
    members: Vec<(String, Vec<u8>)>,
    opts: &Options,
    extract_opts: &ExtractOpts,
    depth: u8,
) -> Vec<PackedFile> {
    let include = if opts.include.is_empty() {
        None
    } else {
        crate::filter::build_globset(&opts.include).ok()
    };
    let Ok(exclude) = crate::filter::build_globset(&opts.exclude) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut total = 0u64;
    for (name, mem_bytes) in members {
        if is_unsafe_entry(&name) {
            continue;
        }
        let child = join_rel(relative, &name);
        if !opts.hidden && crate::filter::is_hidden_rel(&child) {
            continue;
        }
        let n = mem_bytes.len() as u64;
        if total.saturating_add(n) > MAX_ARCHIVE_UNCOMPRESSED {
            break;
        }
        let kind = classify(Path::new(&child), Some(&mem_bytes));
        let emit = !kind.is_archive();
        if emit && !crate::filter::keep_relative(&child, include.as_ref(), &exclude) {
            continue;
        }
        if !emit && glob_exclude_only(&child, &exclude) {
            continue;
        }
        total = total.saturating_add(n);
        out.extend(process_item(
            WorkItem {
                id: format!("{parent_id}!{child}"),
                relative: child,
                absolute: None,
                bytes: mem_bytes,
                depth: depth + 1,
            },
            opts,
            extract_opts,
        ));
    }
    out
}

fn glob_exclude_only(relative: &str, exclude: &globset::GlobSet) -> bool {
    !crate::filter::keep_relative(relative, None, exclude)
}

#[cfg(feature = "native")]
fn list_only_file(entry: &ManifestEntry, opts: &Options) -> PackedFile {
    let kind = entry.kind;
    let status = if entry.size > opts.max_file_size {
        FileStatus::TooLarge(opts.max_file_size)
    } else if kind.is_archive() {
        FileStatus::SkippedArchive
    } else if opts.skip_binaries && kind == Kind::Binary {
        FileStatus::SkippedBinary
    } else {
        FileStatus::Extracted
    };
    PackedFile {
        id: entry.id.clone(),
        relative: entry.relative.clone(),
        kind,
        size: entry.size,
        text: String::new(),
        status,
    }
}

/// Kinds whose extractors read the bytes as text.
fn is_text_like(kind: Kind) -> bool {
    matches!(
        kind,
        Kind::Text | Kind::Html | Kind::Xml | Kind::Json | Kind::Csv | Kind::Tsv | Kind::Unknown
    )
}

fn should_skip_binary(kind: Kind, bytes: &[u8], skip_binaries: bool) -> bool {
    if !skip_binaries {
        return false;
    }
    match kind {
        Kind::Npy | Kind::Npz | Kind::Text => false,
        Kind::Binary => true,
        Kind::Unknown => looks_binary(bytes),
        _ => false,
    }
}

fn render_pack_tree(opts: &Options, files: &[PackedFile]) -> String {
    match opts.tree {
        TreeMode::None => String::new(),
        TreeMode::Selected => {
            let paths: Vec<String> = files
                .iter()
                .filter(|f| f.status == FileStatus::Extracted)
                .map(|f| f.relative.clone())
                .collect();
            crate::tree::render_tree(&tree_label(&opts.roots), &paths)
        }
        TreeMode::Full => {
            let paths: Vec<String> = files.iter().map(|f| f.relative.clone()).collect();
            crate::tree::render_tree(&tree_label(&opts.roots), &paths)
        }
    }
}

/// Label on the first line of the directory map.
///
/// One root is labelled with its directory's name. A file named as the root
/// is drawn inside its directory (`src/` then `└── main.rs`), not as a
/// directory of its own. `..` and `/` name the directory they resolve to.
pub(crate) fn tree_label(roots: &[PathBuf]) -> String {
    match roots {
        [root] => {
            let dir = if root.is_file() {
                root.parent().unwrap_or_else(|| Path::new(""))
            } else {
                root.as_path()
            };
            dir_label(dir)
        }
        _ => "pulp".to_string(),
    }
}

fn dir_label(dir: &Path) -> String {
    if let Some(name) = dir
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
    {
        return name;
    }
    if dir.components().all(|c| matches!(c, Component::CurDir)) {
        return ".".to_string();
    }
    let absolute = if dir.is_absolute() {
        dir.to_path_buf()
    } else {
        match std::env::current_dir() {
            Ok(cwd) => cwd.join(dir),
            Err(_) => return ".".to_string(),
        }
    };
    let mut names: Vec<String> = Vec::new();
    for component in absolute.components() {
        match component {
            Component::Normal(name) => names.push(name.to_string_lossy().into_owned()),
            Component::ParentDir => {
                names.pop();
            }
            _ => {}
        }
    }
    names.pop().unwrap_or_else(|| "/".to_string())
}

fn class_path(item: &WorkItem) -> &Path {
    match item.absolute.as_deref() {
        Some(path) => path,
        None => Path::new(&item.relative),
    }
}

fn is_input_root(path: &Path, roots: &[PathBuf]) -> bool {
    roots.iter().any(|root| {
        if root == path {
            return true;
        }
        match (std::fs::canonicalize(root), std::fs::canonicalize(path)) {
            (Ok(a), Ok(b)) => a == b,
            _ => false,
        }
    })
}

fn is_unsafe_entry(name: &str) -> bool {
    let n = name.replace('\\', "/");
    let n = n.trim();
    if n.is_empty() {
        return true;
    }
    if n.starts_with('/') || n.starts_with('\\') {
        return true;
    }
    let bytes = n.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' {
        return true;
    }
    Path::new(n).components().any(|c| {
        matches!(
            c,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    })
}

fn join_rel(parent: &str, child: &str) -> String {
    let child = normalize_rel(child);
    if parent.is_empty() {
        child
    } else if child.is_empty() {
        parent.to_string()
    } else {
        format!("{parent}/{child}")
    }
}

fn normalize_rel(path: &str) -> String {
    let path = path.replace('\\', "/");
    let mut parts = Vec::new();
    for part in path.split('/') {
        if part.is_empty() || part == "." {
            continue;
        }
        parts.push(part);
    }
    parts.join("/")
}

/// Run an extractor and turn a panic into an error.
///
/// PDF and Office parsers can panic on hostile or unusual files. In the
/// browser that abort surfaces as `unreachable` and drops the whole pack.
fn extract_contained(
    path: Option<&Path>,
    bytes: &[u8],
    kind: Kind,
    relative: &str,
    extract_opts: &ExtractOpts,
) -> Result<String, Error> {
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if crate::extract::isolate::needs_isolation(kind) {
            crate::extract::isolate::extract_heavy(path, bytes, kind, extract_opts)
        } else {
            extract(relative, bytes, kind, extract_opts)
        }
    }));
    match outcome {
        Ok(result) => result,
        Err(payload) => Err(Error::msg(format!(
            "extractor panicked: {}",
            panic_message(payload.as_ref())
        ))),
    }
}

fn packed_too_large(id: String, relative: String, kind: Kind, size: u64, limit: u64) -> PackedFile {
    PackedFile {
        text: format!("[too large: {size} bytes; limit {limit} bytes]"),
        id,
        relative,
        kind,
        size,
        status: FileStatus::TooLarge(limit),
    }
}

fn packed_error(
    id: String,
    relative: String,
    kind: Kind,
    size: u64,
    message: String,
) -> PackedFile {
    PackedFile {
        text: format!("[error extracting {}: {message}]", display_path(&relative)),
        id,
        relative,
        kind,
        size,
        status: FileStatus::Error(message),
    }
}

fn packed_unreadable(
    id: String,
    relative: String,
    kind: Kind,
    size: u64,
    reason: String,
) -> PackedFile {
    PackedFile {
        text: format!("[{} unreadable: {reason}]", kind.as_str()),
        id,
        relative,
        kind,
        size,
        status: FileStatus::Unreadable(reason),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write(path: &Path, body: &[u8]) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, body).unwrap();
    }

    fn list_opts(root: PathBuf) -> Options {
        Options {
            roots: vec![root],
            list_only: true,
            ..Options::default()
        }
    }

    #[test]
    fn test_panic_message_with_str_payload_returns_text() {
        let payload = std::panic::catch_unwind(|| panic!("missing width")).unwrap_err();
        assert_eq!(panic_message(payload.as_ref()), "missing width");
    }

    #[test]
    fn test_file_status_message_with_too_large_includes_size_and_cap() {
        let msg = FileStatus::TooLarge(1024).message(4096);
        assert!(msg.contains("4096"));
        assert!(msg.contains("1024"));
        let err = FileStatus::Error("pdf header missing".into()).message(80);
        assert!(err.contains("pdf header missing"));
        assert!(err.contains("80"));
        assert!(FileStatus::SkippedArchive.message(12).contains("archives"));
    }

    #[test]
    fn test_file_status_with_unreadable_returns_own_label_and_parser_message() {
        let status = FileStatus::Unreadable("invalid file header".into());
        assert_eq!(status.as_str(), "unreadable");
        let msg = status.message(17);
        assert!(msg.contains("17-byte"), "{msg}");
        assert!(msg.contains("one-line note"), "{msg}");
        assert!(msg.ends_with("invalid file header"), "{msg}");
    }

    #[test]
    fn test_pack_manifest_with_progress_returns_one_count_per_entry() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("a.rs"), b"fn a() {}\n");
        write(&dir.path().join("b.rs"), b"fn b() {}\n");
        write(&dir.path().join("paper.pdf"), b"%PDF-1.4 garbage");
        let opts = Options {
            roots: vec![dir.path().to_path_buf()],
            ..Options::default()
        };
        let manifest = crate::manifest::scan_manifest(&opts).unwrap();
        let done = AtomicUsize::new(0);
        let packed = pack_manifest(&manifest, &opts, None, Some(&done), None).unwrap();
        assert_eq!(packed.files.len(), 3);
        assert_eq!(done.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn test_pack_entries_with_empty_only_returns_no_files() {
        let bytes = b"fn x() {}\n";
        let files = [MemoryFile {
            id: "a.rs",
            relative: "a.rs",
            bytes,
        }];
        let opts = Options {
            selection: crate::config::Selection::Only(Vec::new()),
            ..Options::default()
        };
        let packed = pack_entries(&files, &opts, None).unwrap();
        assert!(packed.files.is_empty());
    }

    #[test]
    fn test_pack_entries_with_website_types_extracts_text_and_skips_generated() {
        let math = b"V(h)=1\n";
        let f90 = b"subroutine r\nend\n";
        let gz = [0x1f_u8, 0x8b, 0x08, 0x00];
        let pdf = b"not a pdf";
        let files = [
            MemoryFile {
                id: "index.math",
                relative: "content/index.math",
                bytes: math,
            },
            MemoryFile {
                id: "rates.f90",
                relative: "rates.f90",
                bytes: f90,
            },
            MemoryFile {
                id: "key.asc",
                relative: "public/key.asc",
                bytes: b"-----BEGIN PGP PUBLIC KEY BLOCK-----\n",
            },
            MemoryFile {
                id: "atlas.dat.gz",
                relative: "data/atlas.dat.gz",
                bytes: &gz,
            },
            MemoryFile {
                id: "paper.pdf",
                relative: "paper.pdf",
                bytes: pdf,
            },
            MemoryFile {
                id: "page.js",
                relative: ".next/server/page.js",
                bytes: b"export {}\n",
            },
            MemoryFile {
                id: "built.html",
                relative: "out/index.html",
                bytes: b"<p>built</p>\n",
            },
            MemoryFile {
                id: "sdk.f90",
                relative: "toolchains/sdk/a.f90",
                bytes: b"subroutine a\nend\n",
            },
        ];
        let opts = Options {
            hidden: true,
            ..Options::default()
        };
        let packed = pack_entries(&files, &opts, None).unwrap();
        let math_file = packed
            .files
            .iter()
            .find(|f| f.relative == "content/index.math")
            .unwrap();
        assert_eq!(math_file.status, FileStatus::Extracted);
        assert!(math_file.text.contains("V(h)=1"));
        let f90_file = packed
            .files
            .iter()
            .find(|f| f.relative == "rates.f90")
            .unwrap();
        assert_eq!(f90_file.kind, Kind::Text);
        assert_eq!(f90_file.status, FileStatus::Extracted);
        assert!(f90_file.text.contains("subroutine r"));
        let asc = packed
            .files
            .iter()
            .find(|f| f.relative == "public/key.asc")
            .unwrap();
        assert_eq!(asc.status, FileStatus::Extracted);
        let gz_file = packed
            .files
            .iter()
            .find(|f| f.relative == "data/atlas.dat.gz")
            .unwrap();
        assert_eq!(gz_file.kind, Kind::Binary);
        assert_eq!(gz_file.status, FileStatus::SkippedBinary);
        let pdf_file = packed
            .files
            .iter()
            .find(|f| f.relative == "paper.pdf")
            .unwrap();
        assert!(
            matches!(pdf_file.status, FileStatus::Unreadable(_)),
            "{:?}",
            pdf_file.status
        );
        assert!(
            pdf_file.text.starts_with("[pdf unreadable: "),
            "{}",
            pdf_file.text
        );
        let rels: Vec<&str> = packed.files.iter().map(|f| f.relative.as_str()).collect();
        assert!(!rels.iter().any(|r| r.contains(".next")), "{rels:?}");
        assert!(!rels.iter().any(|r| r.starts_with("out/")), "{rels:?}");
        assert!(!rels.iter().any(|r| r.contains("toolchains")), "{rels:?}");
    }

    #[test]
    fn test_pack_entries_with_exclude_key_skips_key_file() {
        let rs = b"fn x() {}\n";
        let key = b"SECRET\n";
        let files = [
            MemoryFile {
                id: "a.rs",
                relative: "a.rs",
                bytes: rs,
            },
            MemoryFile {
                id: "a.key",
                relative: "a.key",
                bytes: key,
            },
        ];
        let opts = Options {
            exclude: vec!["*.key".into()],
            selection: crate::config::Selection::AllEligible,
            ..Options::default()
        };
        let packed = pack_entries(&files, &opts, None).unwrap();
        let rels: Vec<&str> = packed.files.iter().map(|f| f.relative.as_str()).collect();
        assert!(rels.contains(&"a.rs"), "{rels:?}");
        assert!(!rels.contains(&"a.key"), "{rels:?}");
    }

    #[test]
    fn test_pack_entries_preserves_distinct_ids() {
        let bytes = b"fn x() {}\n";
        let files = [
            MemoryFile {
                id: "0:repo/src/lib.rs",
                relative: "repo/src/lib.rs",
                bytes,
            },
            MemoryFile {
                id: "1:repo/src/lib.rs",
                relative: "repo/src/lib.rs",
                bytes,
            },
        ];
        let opts = Options {
            selection: crate::config::Selection::AllEligible,
            ..Options::default()
        };
        let packed = pack_entries(&files, &opts, None).unwrap();
        let ids: Vec<&str> = packed.files.iter().map(|f| f.id.as_str()).collect();
        assert!(ids.contains(&"0:repo/src/lib.rs"), "{ids:?}");
        assert!(ids.contains(&"1:repo/src/lib.rs"), "{ids:?}");
    }

    #[test]
    fn test_pack_with_missing_root_returns_path_error() {
        let opts = Options {
            roots: vec![PathBuf::from("/nonexistent/pulp-pack-missing-root")],
            ..Options::default()
        };
        let err = pack(&opts).unwrap_err();
        assert!(matches!(err, Error::Path { .. }));
    }

    #[test]
    fn test_pack_with_list_only_skips_node_modules() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("src/lib.rs"), b"pub fn x() {}\n");
        write(
            &dir.path().join("node_modules/pkg/index.js"),
            b"module.exports=1;\n",
        );
        let packed = pack(&list_opts(dir.path().to_path_buf())).unwrap();
        let rels: Vec<&str> = packed.files.iter().map(|f| f.relative.as_str()).collect();
        assert!(rels.contains(&"src/lib.rs"));
        assert!(
            !rels
                .iter()
                .any(|r| r.split('/').any(|p| p == "node_modules"))
        );
        assert!(packed.files.iter().all(|f| f.text.is_empty()));
    }

    #[test]
    fn test_pack_with_rs_lean_npz_does_not_skip_as_binary() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("src/lib.rs"), b"pub fn x() {}\n");
        write(&dir.path().join("Math/Basic.lean"), b"def x := 1\n");
        write(&dir.path().join("arrays/params.npz"), &[0, 1, 2, 3, 4]);
        write(&dir.path().join("pic.png"), &[0x89, b'P', b'N', b'G']);
        write(&dir.path().join("target/debug/foo.rs"), b"fn main() {}\n");

        let packed = pack(&list_opts(dir.path().to_path_buf())).unwrap();
        let by_rel: Vec<(&str, Kind, FileStatus)> = packed
            .files
            .iter()
            .map(|f| (f.relative.as_str(), f.kind, f.status.clone()))
            .collect();

        assert!(by_rel.iter().any(|(r, k, s)| {
            *r == "src/lib.rs" && *k == Kind::Text && *s == FileStatus::Extracted
        }));
        assert!(by_rel.iter().any(|(r, k, s)| {
            *r == "Math/Basic.lean" && *k == Kind::Text && *s == FileStatus::Extracted
        }));
        assert!(by_rel.iter().any(|(r, k, s)| {
            *r == "arrays/params.npz" && *k == Kind::Npz && *s == FileStatus::Extracted
        }));
        assert!(by_rel.iter().any(|(r, k, s)| {
            *r == "pic.png" && *k == Kind::Binary && *s == FileStatus::SkippedBinary
        }));
        assert!(
            !packed
                .files
                .iter()
                .any(|f| f.relative.starts_with("target/"))
        );
    }

    #[test]
    fn test_pack_with_archive_excludes_hidden_env_member() {
        use std::io::{Cursor, Write as IoWrite};
        use zip::ZipWriter;
        use zip::write::SimpleFileOptions;

        let dir = tempfile::tempdir().unwrap();
        let zip_path = dir.path().join("bundle.zip");
        let mut buf = Cursor::new(Vec::new());
        {
            let mut zw = ZipWriter::new(&mut buf);
            let opt = SimpleFileOptions::default();
            zw.start_file("src/lib.rs", opt).unwrap();
            zw.write_all(b"pub fn x() {}\n").unwrap();
            zw.start_file(".env", opt).unwrap();
            zw.write_all(b"SECRET=1\n").unwrap();
            zw.finish().unwrap();
        }
        std::fs::write(&zip_path, buf.into_inner()).unwrap();
        let opts = Options {
            roots: vec![zip_path],
            follow_archives: true,
            hidden: false,
            ..Options::default()
        };
        let packed = pack(&opts).unwrap();
        let rels: Vec<&str> = packed.files.iter().map(|f| f.relative.as_str()).collect();
        assert!(
            rels.iter().any(|r| r.ends_with("lib.rs")),
            "expected rust member, got {rels:?}"
        );
        assert!(
            !rels.iter().any(|r| r.contains(".env")),
            ".env must not leak from archives, got {rels:?}"
        );
    }

    #[test]
    fn test_pack_manifest_entry_with_one_entry_returns_its_text_only() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("a.rs"), b"fn a() {}\n");
        write(&dir.path().join("b.rs"), b"fn b() {}\n");
        let opts = Options {
            roots: vec![dir.path().to_path_buf()],
            ..Options::default()
        };
        let manifest = crate::manifest::scan_manifest(&opts).unwrap();
        let entry = manifest
            .entries
            .iter()
            .find(|e| e.relative == "b.rs")
            .unwrap();
        let files = pack_manifest_entry(entry, &opts);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].relative, "b.rs");
        assert_eq!(files[0].status, FileStatus::Extracted);
        assert!(files[0].text.contains("fn b"), "{}", files[0].text);
    }

    #[test]
    fn test_pack_manifest_entry_with_zip_returns_sorted_members() {
        use std::io::{Cursor, Write as IoWrite};
        use zip::ZipWriter;
        use zip::write::SimpleFileOptions;

        let dir = tempfile::tempdir().unwrap();
        let mut buf = Cursor::new(Vec::new());
        {
            let mut zw = ZipWriter::new(&mut buf);
            let opt = SimpleFileOptions::default();
            zw.start_file("z.rs", opt).unwrap();
            zw.write_all(b"fn z() {}\n").unwrap();
            zw.start_file("a.rs", opt).unwrap();
            zw.write_all(b"fn a() {}\n").unwrap();
            zw.finish().unwrap();
        }
        write(&dir.path().join("bundle.zip"), &buf.into_inner());
        let opts = Options {
            roots: vec![dir.path().to_path_buf()],
            follow_archives: true,
            ..Options::default()
        };
        let manifest = crate::manifest::scan_manifest(&opts).unwrap();
        assert_eq!(manifest.entries.len(), 1);
        let files = pack_manifest_entry(&manifest.entries[0], &opts);
        let rels: Vec<&str> = files.iter().map(|f| f.relative.as_str()).collect();
        assert_eq!(rels, vec!["bundle.zip/a.rs", "bundle.zip/z.rs"]);
    }

    #[test]
    fn test_pack_with_empty_only_returns_no_files() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("src/lib.rs"), b"pub fn x() {}\n");
        let opts = Options {
            roots: vec![dir.path().to_path_buf()],
            selection: crate::config::Selection::Only(Vec::new()),
            ..Options::default()
        };
        let packed = pack(&opts).unwrap();
        assert!(packed.files.is_empty());
        assert!(!packed.stats.truncated);
    }

    #[test]
    fn test_pack_entries_with_no_entry_cap_keeps_every_file() {
        let a = b"fn a() {}\n";
        let b = b"fn b() {}\n";
        let files = [
            MemoryFile {
                id: "a.rs",
                relative: "a.rs",
                bytes: a,
            },
            MemoryFile {
                id: "b.rs",
                relative: "b.rs",
                bytes: b,
            },
        ];
        let opts = Options {
            max_entries: 0,
            exclude: Vec::new(),
            ..Options::default()
        };
        let packed = pack_entries(&files, &opts, None).unwrap();
        assert_eq!(packed.files.len(), 2);
        assert!(!packed.stats.truncated);
    }

    #[test]
    fn test_pack_with_max_entries_sets_truncated() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("a.rs"), b"fn a() {}\n");
        write(&dir.path().join("b.rs"), b"fn b() {}\n");
        let opts = Options {
            roots: vec![dir.path().to_path_buf()],
            max_entries: 1,
            list_only: true,
            ..Options::default()
        };
        let packed = pack(&opts).unwrap();
        assert_eq!(packed.files.len(), 1);
        assert!(packed.stats.truncated);
    }

    #[test]
    fn test_is_unsafe_entry_with_parent_or_abs_returns_true() {
        assert!(is_unsafe_entry("../etc/passwd"));
        assert!(is_unsafe_entry("/etc/passwd"));
        assert!(is_unsafe_entry("foo/../../bar"));
        assert!(is_unsafe_entry("C:/Windows/system32"));
        assert!(!is_unsafe_entry("foo/bar.txt"));
        assert!(!is_unsafe_entry("dir/file.rs"));
    }

    fn stored_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        use std::io::{Cursor, Write as IoWrite};
        let mut zw = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let opt = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        for (name, data) in entries {
            zw.start_file(*name, opt).unwrap();
            zw.write_all(data).unwrap();
        }
        zw.finish().unwrap().into_inner()
    }

    #[test]
    fn test_tree_label_with_file_and_dot_roots_returns_directory_names() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        write(&src.join("main.rs"), b"fn main() {}\n");
        assert_eq!(tree_label(&[src.join("main.rs")]), "src");
        assert_eq!(tree_label(std::slice::from_ref(&src)), "src");
        assert_eq!(tree_label(&[src.join("..")]), dir_label(dir.path()));
        assert_eq!(tree_label(&[PathBuf::from(".")]), ".");
        assert_eq!(tree_label(&[PathBuf::from("/")]), "/");
        assert_eq!(tree_label(&[src.clone(), src]), "pulp");

        let cwd = std::env::current_dir().unwrap();
        let parent = cwd.parent().unwrap().file_name().unwrap();
        assert_eq!(tree_label(&[PathBuf::from("..")]), parent.to_string_lossy());
    }

    #[test]
    fn test_pack_with_single_file_root_draws_file_inside_its_directory() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("proj/main.rs");
        write(&file, b"fn main() {}\n");
        let packed = pack(&Options {
            roots: vec![file],
            ..Options::default()
        })
        .unwrap();
        assert_eq!(packed.tree, "proj/\n└── main.rs\n");
    }

    #[test]
    fn test_pack_entries_with_binary_bytes_under_text_names_skips_them() {
        let ts: Vec<u8> = [0x47, 0x40, 0x00, 0x10, 0x00, 0x00, 0xb0, 0x0d]
            .into_iter()
            .chain((0..=255u8).cycle().take(1024))
            .collect();
        let utf16: Vec<u8> = "hello from utf-16\n"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        let entries = [
            MemoryFile {
                id: "clip.ts",
                relative: "clip.ts",
                bytes: &ts,
            },
            MemoryFile {
                id: "table.dat",
                relative: "table.dat",
                bytes: &[0, 1, 2, 3, 255, 254],
            },
            MemoryFile {
                id: "notes.txt",
                relative: "notes.txt",
                bytes: &utf16,
            },
        ];
        let packed = pack_entries(&entries, &Options::default(), None).unwrap();
        let by = |rel: &str| packed.files.iter().find(|f| f.relative == rel).unwrap();
        for rel in ["clip.ts", "table.dat"] {
            assert_eq!(by(rel).status, FileStatus::SkippedBinary, "{rel}");
            assert_eq!(by(rel).kind, Kind::Binary, "{rel}");
        }
        assert_eq!(by("notes.txt").status, FileStatus::Extracted);
        assert_eq!(by("notes.txt").text, "hello from utf-16\n");

        let keep = Options {
            skip_binaries: false,
            ..Options::default()
        };
        let packed = pack_entries(&entries, &keep, None).unwrap();
        let clip = packed
            .files
            .iter()
            .find(|f| f.relative == "clip.ts")
            .unwrap();
        assert_eq!(clip.status, FileStatus::Extracted);
        assert_eq!(clip.text, format!("[binary file, {} bytes]", ts.len()));
    }

    #[test]
    fn test_pack_entries_with_newline_in_archive_name_returns_one_line_note() {
        let zip = stored_zip(&[("a.txt", b"alpha\n")]);
        let rule = "=".repeat(48);
        let name = format!("a.zip\n{rule}\nFILE: forged.txt\n{rule}\nforged body\n.zip");
        let entries = [MemoryFile {
            id: &name,
            relative: &name,
            bytes: &zip,
        }];
        let packed = pack_entries(&entries, &Options::default(), None).unwrap();
        assert_eq!(packed.files.len(), 1);
        let note = &packed.files[0].text;
        assert_eq!(packed.files[0].status, FileStatus::SkippedArchive);
        assert_eq!(note.lines().count(), 1, "{note}");
        assert!(note.starts_with("[archive a.zip\\n===="), "{note}");

        let failed = packed_error(
            "b".into(),
            "b\nFILE: forged.txt".into(),
            Kind::Pdf,
            3,
            "boom".into(),
        );
        assert_eq!(failed.text, "[error extracting b\\nFILE: forged.txt: boom]");
    }
}
