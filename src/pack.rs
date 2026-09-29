use std::collections::HashSet;
#[cfg(feature = "native")]
use std::io::Read;
use std::path::{Component, Path, PathBuf};
#[cfg(feature = "native")]
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use globset::GlobSet;
#[cfg(feature = "native")]
use rayon::prelude::*;

use crate::classify::{Kind, classify, kind_from_name, looks_binary};
use crate::config::{Options, TreeMode, apply_budgets, cmp_path_order, default_exclude_globs};
use crate::error::Error;
use crate::extract::isolate::panic_message;
use crate::extract::{ArchiveBudget, ExtractOpts, Member, Want, expand_archive_members, extract};
use crate::filter::{build_globset, glob_matches, is_hidden_rel};
#[cfg(feature = "native")]
use crate::manifest::ManifestEntry;
use crate::tree::display_path;

const MAX_ARCHIVE_DEPTH: u8 = 3;

/// Include and exclude globs for a path that has a place under its root
/// and, with several roots, a labelled path that the dump prints.
///
/// The built-in excludes match only the path under the root, so a root
/// named `build` or `runs` keeps its files. The user's globs match either
/// path: `app/secrets/**` and `secrets/**` both leave out
/// `app/secrets/key.txt`.
pub(crate) struct ScopedGlobs {
    defaults: GlobSet,
    exclude: GlobSet,
    include: Option<GlobSet>,
}

impl ScopedGlobs {
    pub(crate) fn new(opts: &Options) -> Result<Self, Error> {
        let defaults = if opts.default_excludes {
            build_globset(&default_exclude_globs())?
        } else {
            GlobSet::empty()
        };
        let include = if opts.include.is_empty() {
            None
        } else {
            Some(build_globset(&opts.include)?)
        };
        Ok(Self {
            defaults,
            exclude: build_globset(&opts.exclude)?,
            include,
        })
    }

    /// Whether a path passes. `labelled` is the path the dump prints, the
    /// same as `under_root` with one root. An archive about to be unpacked
    /// passes the include globs, since its members are judged one by one.
    pub(crate) fn keep(&self, under_root: &str, labelled: &str, unpacking: bool) -> bool {
        let either = |set: &GlobSet| {
            glob_matches(set, under_root) || (labelled != under_root && glob_matches(set, labelled))
        };
        if glob_matches(&self.defaults, under_root) || either(&self.exclude) {
            return false;
        }
        match &self.include {
            None => true,
            Some(set) => unpacking || either(set),
        }
    }
}

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
    /// Path under its root, which the globs and the hidden rule judge; the
    /// same as `relative` with one root.
    root_relative: String,
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
    let archive_cut = AtomicBool::new(false);

    let mut files = run_parallel(opts.jobs, || {
        manifest
            .entries
            .par_iter()
            .flat_map(|entry| {
                let (files, cut) = process_entry(entry, opts, &extract_opts, &bytes_read, cancel);
                if cut {
                    archive_cut.store(true, Ordering::Relaxed);
                }
                if let Some(done) = progress {
                    done.fetch_add(1, Ordering::SeqCst);
                }
                files
            })
            .collect::<Vec<PackedFile>>()
    })?;
    files.sort_by(|a, b| a.relative.cmp(&b.relative));
    let cancelled = cancel.is_some_and(|c| c.load(Ordering::Relaxed));
    let archive_cut = archive_cut.load(Ordering::Relaxed);

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
            truncated: manifest.truncated || cancelled || archive_cut,
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
    let (mut files, _) = process_entry(entry, opts, &extract_opts, &AtomicU64::new(0), None);
    files.sort_by(|a, b| a.relative.cmp(&b.relative));
    files
}

/// Pack an in-memory file set (browser / virtual trees). No filesystem reads.
///
/// Enforces [`Options::selection`] (see [`crate::config::SelectionFilter`]),
/// include/exclude, hidden, and the entry and byte budgets. The budgets are
/// spent in path order whatever order `entries` come in, so they keep the
/// files a walk of the same folder keeps (see [`crate::config::Budget`]).
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
    let selection = opts.selection.resolve(entries.iter().map(|entry| entry.id));
    let mut chosen: Vec<&MemoryFile<'_>> = entries
        .iter()
        .filter(|entry| {
            selection.contains(entry.id, entry.relative) && policy.keep_walk(entry.relative)
        })
        .collect();
    chosen.sort_by(|a, b| cmp_path_order(a.relative, b.relative).then_with(|| a.id.cmp(b.id)));
    let mut truncated = apply_budgets(&mut chosen, opts, |entry| entry.bytes.len() as u64);
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
        let mut budget = ArchiveBudget::default();
        files.extend(process_item(
            WorkItem {
                id: entry.id.to_string(),
                relative: entry.relative.to_string(),
                root_relative: entry.relative.to_string(),
                absolute: None,
                bytes: entry.bytes.to_vec(),
                depth: 0,
            },
            opts,
            &extract_opts,
            &mut budget,
        ));
        truncated |= budget.cut();
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
) -> (Vec<PackedFile>, bool) {
    if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
        return (Vec::new(), false);
    }
    if opts.list_only {
        return (vec![list_only_file(entry, opts)], false);
    }
    if let Some(msg) = changed_since_scan(entry) {
        return (vec![packed_changed(entry, msg)], false);
    }
    if entry.size > opts.max_file_size {
        let too_large = packed_too_large(
            entry.id.clone(),
            entry.relative.clone(),
            entry.kind,
            entry.size,
            opts.max_file_size,
        );
        return (vec![too_large], false);
    }
    let bytes = match read_limited(entry, opts.follow_links, opts.max_file_size) {
        Ok(EntryRead::Bytes(bytes)) => bytes,
        Ok(EntryRead::TooLarge(size)) => {
            let too_large = packed_too_large(
                entry.id.clone(),
                entry.relative.clone(),
                entry.kind,
                size,
                opts.max_file_size,
            );
            return (vec![too_large], false);
        }
        Ok(EntryRead::Changed(msg)) => return (vec![packed_changed(entry, msg)], false),
        Err(err) => {
            let failed = packed_error(
                entry.id.clone(),
                entry.relative.clone(),
                entry.kind,
                entry.size,
                err.to_string(),
            );
            return (vec![failed], false);
        }
    };
    bytes_read.fetch_add(bytes.len() as u64, Ordering::Relaxed);
    let mut budget = ArchiveBudget::default();
    let files = process_item(
        WorkItem {
            id: entry.id.clone(),
            relative: entry.relative.clone(),
            root_relative: entry.root_relative.clone(),
            absolute: Some(entry.absolute.clone()),
            bytes,
            depth: 0,
        },
        opts,
        extract_opts,
        &mut budget,
    );
    (files, budget.cut())
}

/// What reading a scanned entry found.
#[cfg(feature = "native")]
enum EntryRead {
    Bytes(Vec<u8>),
    /// Grew past the per-file cap; holds its size.
    TooLarge(u64),
    /// Not the file the scan saw; holds why.
    Changed(String),
}

/// Read the file behind `entry`, up to `max` bytes.
///
/// The file is opened first, refusing the swaps [`open_entry`] names, and
/// its type, size, and modification time are then checked on the open
/// file, so nothing done to the path after the open changes what is read.
#[cfg(feature = "native")]
fn read_limited(entry: &ManifestEntry, follow_links: bool, max: u64) -> std::io::Result<EntryRead> {
    let file = match open_entry(entry, follow_links)? {
        Ok(file) => file,
        Err(msg) => return Ok(EntryRead::Changed(msg.into())),
    };
    if let Some(msg) = changed_from(entry, &file.metadata()?) {
        return Ok(EntryRead::Changed(msg));
    }
    let mut buf = Vec::new();
    let n = (&file).take(max.saturating_add(1)).read_to_end(&mut buf)?;
    if n as u64 > max {
        let size = file.metadata().map_or(n as u64, |meta| meta.len());
        return Ok(EntryRead::TooLarge(size));
    }
    Ok(EntryRead::Bytes(buf))
}

/// Open the file behind `entry` for reading, or say how its path changed
/// since the scan.
///
/// On Unix, unless the scan followed links, no symlink is followed anywhere
/// between the scanned root and the file (see
/// [`crate::walk::open_beneath`]): a folder on the way that became a
/// symlink to another folder cannot pass that folder's file off as this
/// one. With `--follow-links`, or elsewhere, the file's own name is guarded,
/// and a symlink the scan found there is followed.
#[cfg(feature = "native")]
fn open_entry(
    entry: &ManifestEntry,
    follow_links: bool,
) -> std::io::Result<Result<std::fs::File, &'static str>> {
    #[cfg(unix)]
    if !follow_links && !entry.is_symlink {
        use crate::walk::PathChange;
        let depth = entry.root_relative.split('/').count();
        let opened = crate::walk::open_beneath(&entry.absolute, depth)?;
        return Ok(opened.map_err(|change| match change {
            PathChange::Symlink => "changed since scan: replaced by a symlink",
            PathChange::Gone => "changed since scan: no longer there",
            PathChange::NotFolder => {
                "changed since scan: a folder on its path is no longer a folder"
            }
        }));
    }
    #[cfg(not(unix))]
    let _ = follow_links;
    match crate::walk::open_for_read(&entry.absolute, entry.is_symlink) {
        Ok(file) => Ok(Ok(file)),
        Err(err) => {
            let now_link = std::fs::symlink_metadata(&entry.absolute)
                .is_ok_and(|meta| meta.file_type().is_symlink());
            if now_link && !entry.is_symlink {
                return Ok(Err("changed since scan: replaced by a symlink"));
            }
            Err(err)
        }
    }
}

/// Pack one input or archive member. Archives expand in place, drawing on
/// `budget`, which every archive nested under one top-level input shares.
fn process_item(
    item: WorkItem,
    opts: &Options,
    extract_opts: &ExtractOpts,
    budget: &mut ArchiveBudget,
) -> Vec<PackedFile> {
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
        return expand_item(&item, kind, opts, extract_opts, budget);
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

/// Why the file at `entry.absolute` is no longer the one the scan saw, if
/// it is not, judged from its path before anything opens it, so a path that
/// has become a device or a FIFO is never opened. A path that has gone is
/// left for the read to report.
#[cfg(feature = "native")]
fn changed_since_scan(entry: &ManifestEntry) -> Option<String> {
    let link = std::fs::symlink_metadata(&entry.absolute).ok()?;
    if link.file_type().is_symlink() != entry.is_symlink {
        return Some(if entry.is_symlink {
            "changed since scan: no longer a symlink".into()
        } else {
            "changed since scan: replaced by a symlink".into()
        });
    }
    let meta = if entry.is_symlink {
        std::fs::metadata(&entry.absolute).ok()?
    } else {
        link
    };
    changed_from(entry, &meta)
}

/// Why a file whose metadata is `meta` is not the one the scan saw.
///
/// The type, size, and modification time decide, as they do for `make`.
/// The device and inode do not: a remount, a network share reconnecting
/// after sleep, or an overlay copy-up gives unchanged files new ones.
#[cfg(feature = "native")]
fn changed_from(entry: &ManifestEntry, meta: &std::fs::Metadata) -> Option<String> {
    if !meta.is_file() {
        return Some("changed since scan: no longer a regular file".into());
    }
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

#[cfg(feature = "native")]
fn packed_changed(entry: &ManifestEntry, msg: String) -> PackedFile {
    PackedFile {
        id: entry.id.clone(),
        relative: entry.relative.clone(),
        kind: entry.kind,
        size: entry.size,
        text: format!("[{msg}]"),
        status: FileStatus::Changed,
    }
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
        return packed_binary(id, relative, kind, size);
    }
    if kind.is_archive() {
        // Names in a note are escaped as in a header, so one cannot end the
        // note's line and forge a section of its own.
        let name = display_path(&relative);
        let text = if opts.follow_archives {
            format!(
                "[archive {name}, {size} bytes; not expanded: nested more than {MAX_ARCHIVE_DEPTH} archives deep]"
            )
        } else {
            format!("[archive {name}, {size} bytes; pass --archives to expand nested archives]")
        };
        return PackedFile {
            text,
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

/// Expand an archive into packed members. An archive its parser rejects
/// keeps a one-line note in its place, like any other unreadable file.
fn expand_item(
    item: &WorkItem,
    kind: Kind,
    opts: &Options,
    extract_opts: &ExtractOpts,
    budget: &mut ArchiveBudget,
) -> Vec<PackedFile> {
    let Ok(globs) = ScopedGlobs::new(opts) else {
        return Vec::new();
    };
    let want = |name: &str| member_want(item, name, opts, &globs);
    match expand_archive_members(&item.bytes, kind, extract_opts, budget, &want) {
        Ok(members) => take_archive_members(item, members, opts, extract_opts, budget, &globs),
        Err(err) => vec![packed_unreadable(
            item.id.clone(),
            item.relative.clone(),
            kind,
            item.bytes.len() as u64,
            err.to_string(),
        )],
    }
}

/// What to take from an archive member, judged by its name alone the way
/// [`take_archive_members`] judges it once read, so a member the filters
/// drop is never read or charged to the budget.
fn member_want(parent: &WorkItem, name: &str, opts: &Options, globs: &ScopedGlobs) -> Want {
    if is_unsafe_entry(name) {
        return Want::Nothing;
    }
    let child = join_rel(&parent.relative, name);
    let child_under_root = join_rel(&parent.root_relative, name);
    if !opts.hidden && is_hidden_rel(&child_under_root) {
        return Want::Nothing;
    }
    // A name that leaves the kind to the bytes may hold an archive, which
    // the include globs let through.
    let named = kind_from_name(Path::new(&child));
    if !globs.keep(
        &child_under_root,
        &child,
        named.is_none_or(Kind::is_archive),
    ) {
        return Want::Nothing;
    }
    if named == Some(Kind::Binary) && opts.skip_binaries {
        return Want::Size;
    }
    Want::Bytes
}

fn take_archive_members(
    parent: &WorkItem,
    members: Vec<Member>,
    opts: &Options,
    extract_opts: &ExtractOpts,
    budget: &mut ArchiveBudget,
    globs: &ScopedGlobs,
) -> Vec<PackedFile> {
    let mut out = Vec::new();
    let mut ids: HashSet<String> = HashSet::new();
    for member in members {
        if is_unsafe_entry(member.name()) {
            continue;
        }
        let child = join_rel(&parent.relative, member.name());
        // Judged by its path under the root, as the walk judges files, so a
        // root named `runs` or `.dotfiles` keeps its archives' members.
        let child_under_root = join_rel(&parent.root_relative, member.name());
        if !opts.hidden && is_hidden_rel(&child_under_root) {
            continue;
        }
        let sniff = match &member {
            Member::File { bytes, .. } => Some(bytes.as_slice()),
            _ => None,
        };
        let kind = classify(Path::new(&child), sniff);
        if !globs.keep(&child_under_root, &child, kind.is_archive()) {
            continue;
        }
        // A tar may hold one name twice; each copy still needs its own id.
        let base = format!("{}!{child}", parent.id);
        let mut id = base.clone();
        let mut n = 2usize;
        while !ids.insert(id.clone()) {
            id = format!("{base}#{n}");
            n += 1;
        }
        match member {
            Member::TooLarge { size, .. } => {
                out.push(packed_too_large(id, child, kind, size, opts.max_file_size));
            }
            Member::Unread { size, .. } => out.push(packed_binary(id, child, kind, size)),
            Member::Unreadable { size, reason, .. } => {
                out.push(packed_unreadable(id, child, kind, size, reason));
            }
            Member::File { bytes, .. } => out.extend(process_item(
                WorkItem {
                    id,
                    relative: child,
                    root_relative: child_under_root,
                    absolute: None,
                    bytes,
                    depth: parent.depth + 1,
                },
                opts,
                extract_opts,
                budget,
            )),
        }
    }
    out
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
#[must_use]
pub fn tree_label(roots: &[PathBuf]) -> String {
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

/// True when an archive member name could climb out of its archive.
///
/// Names reach here with `/` separators already (see
/// [`crate::extract::expand_archive`]); a `\` left in a name is a character,
/// such as the start of a `\xNN` escape for a byte that is not UTF-8.
fn is_unsafe_entry(name: &str) -> bool {
    let n = name.trim();
    if n.is_empty() || n.starts_with('/') {
        return true;
    }
    let bytes = n.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        return true;
    }
    n.split('/').any(|part| part == "..")
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

/// The note for a binary file left out, like `--skip-binaries` leaves them.
fn packed_binary(id: String, relative: String, kind: Kind, size: u64) -> PackedFile {
    PackedFile {
        text: format!("[binary file, {size} bytes]"),
        id,
        relative,
        kind,
        size,
        status: FileStatus::SkippedBinary,
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
    fn test_pack_entries_with_one_path_under_two_ids_returns_both_ids() {
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
    fn test_pack_with_multiple_roots_named_like_excluded_dirs_keeps_archive_members() {
        let dir = tempfile::tempdir().unwrap();
        let runs = dir.path().join("runs");
        let dotfiles = dir.path().join(".dotfiles");
        let notes = dir.path().join("notes");
        write(
            &runs.join("data.zip"),
            &stored_zip(&[("a.txt", b"alpha\n")]),
        );
        write(
            &dotfiles.join("cfg.zip"),
            &stored_zip(&[("b.txt", b"beta\n")]),
        );
        write(&notes.join("n.md"), b"# n\n");
        let opts = Options {
            roots: vec![runs, dotfiles, notes],
            follow_archives: true,
            ..Options::default()
        };
        let packed = pack(&opts).unwrap();
        let rels: Vec<&str> = packed.files.iter().map(|f| f.relative.as_str()).collect();
        assert_eq!(
            rels,
            [
                ".dotfiles/cfg.zip/b.txt",
                "notes/n.md",
                "runs/data.zip/a.txt"
            ]
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
            default_excludes: false,
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
        assert!(!is_unsafe_entry("\\xffstart.txt"));
        assert!(!is_unsafe_entry("notes..v2.txt"));
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

    fn mem_item(relative: &str, bytes: Vec<u8>) -> WorkItem {
        WorkItem {
            id: relative.to_string(),
            relative: relative.to_string(),
            root_relative: relative.to_string(),
            absolute: None,
            bytes,
            depth: 0,
        }
    }

    /// A manifest entry the scan made for a regular file at `path`, with
    /// its current size and modification time.
    #[cfg(unix)]
    fn scanned_entry(path: &Path) -> ManifestEntry {
        let meta = fs::metadata(path).unwrap();
        ManifestEntry {
            id: "a.txt".into(),
            relative: "a.txt".into(),
            root_relative: "a.txt".into(),
            absolute: path.to_path_buf(),
            size: meta.len(),
            kind: Kind::Text,
            language: "text".into(),
            default_on: true,
            oversized: false,
            is_symlink: false,
            modified: meta.modified().ok(),
        }
    }

    /// Scan `dir`, then let `swap` replace `name` before the pack reads it.
    /// Returns what packing that entry gives, failing if it blocks.
    #[cfg(unix)]
    fn pack_after_swap(dir: &Path, name: &str, swap: impl FnOnce(&Path)) -> Vec<PackedFile> {
        let opts = Options {
            roots: vec![dir.to_path_buf()],
            ..Options::default()
        };
        let manifest = crate::manifest::scan_manifest(&opts).unwrap();
        let entry = manifest
            .entries
            .iter()
            .find(|e| e.relative == name)
            .unwrap()
            .clone();
        swap(&dir.join(name));
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(pack_manifest_entry(&entry, &opts));
        });
        rx.recv_timeout(Duration::from_secs(20))
            .expect("packing a swapped file must not block")
    }

    /// Give `path` the modification time of `reference`, following links.
    #[cfg(unix)]
    fn copy_mtime(reference: &Path, path: &Path) {
        let status = std::process::Command::new("touch")
            .arg("-r")
            .arg(reference)
            .arg(path)
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[cfg(unix)]
    #[test]
    fn test_pack_manifest_entry_with_file_swapped_for_fifo_returns_changed_without_blocking() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("x.txt"), b"");
        let keep = dir.path().join("keep");
        let files = pack_after_swap(dir.path(), "x.txt", |path| {
            fs::rename(path, &keep).unwrap();
            let made = std::process::Command::new("mkfifo")
                .arg(path)
                .status()
                .unwrap();
            assert!(made.success());
            copy_mtime(&keep, path);
        });
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].status, FileStatus::Changed, "{:?}", files[0]);
        assert!(
            files[0].text.contains("no longer a regular file"),
            "{}",
            files[0].text
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_read_limited_with_fifo_after_the_check_returns_changed_without_blocking() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        write(&path, b"");
        let entry = scanned_entry(&path);
        // The swap lands after the path check, so only the read can see it.
        fs::remove_file(&path).unwrap();
        let made = std::process::Command::new("mkfifo")
            .arg(&path)
            .status()
            .unwrap();
        assert!(made.success());
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(match read_limited(&entry, false, 1024) {
                Ok(EntryRead::Changed(msg)) => msg,
                Ok(_) => "read".into(),
                Err(err) => err.to_string(),
            });
        });
        let msg = rx
            .recv_timeout(Duration::from_secs(20))
            .expect("open blocked");
        assert!(msg.contains("no longer a regular file"), "{msg}");
    }

    #[cfg(unix)]
    #[test]
    fn test_read_limited_with_symlink_swapped_in_after_the_check_returns_changed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        write(&path, b"scanned\n");
        let entry = scanned_entry(&path);
        let elsewhere = tempfile::tempdir().unwrap();
        let secret = elsewhere.path().join("secret.txt");
        write(&secret, b"private\n");
        copy_mtime(&path, &secret);
        fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink(&secret, &path).unwrap();
        match read_limited(&entry, false, 1024) {
            Ok(EntryRead::Changed(msg)) => assert!(msg.contains("replaced by a symlink"), "{msg}"),
            Ok(EntryRead::Bytes(bytes)) => panic!("read {:?}", String::from_utf8_lossy(&bytes)),
            Ok(EntryRead::TooLarge(size)) => panic!("too large: {size}"),
            Err(err) => panic!("{err}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn test_changed_since_scan_with_same_size_and_mtime_on_new_inode_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        write(&path, b"scanned\n");
        let entry = scanned_entry(&path);
        // The same bytes, size, and time on a new inode: what a remount or
        // an overlay copy-up leaves behind.
        let copy = dir.path().join("a.copy");
        fs::copy(&path, &copy).unwrap();
        copy_mtime(&path, &copy);
        fs::rename(&copy, &path).unwrap();
        assert_eq!(changed_since_scan(&entry), None);
        match read_limited(&entry, false, 1024).unwrap() {
            EntryRead::Bytes(bytes) => assert_eq!(bytes, b"scanned\n"),
            EntryRead::Changed(msg) => panic!("{msg}"),
            EntryRead::TooLarge(size) => panic!("too large: {size}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn test_pack_manifest_entry_with_file_swapped_for_symlink_returns_changed() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("a.txt"), b"scanned\n");
        let elsewhere = tempfile::tempdir().unwrap();
        let other = elsewhere.path().join("secret.txt");
        write(&other, b"private\n");
        let files = pack_after_swap(dir.path(), "a.txt", |path| {
            copy_mtime(path, &other);
            fs::remove_file(path).unwrap();
            std::os::unix::fs::symlink(&other, path).unwrap();
        });
        assert_eq!(files[0].status, FileStatus::Changed, "{:?}", files[0]);
        assert!(!files[0].text.contains("private"), "{}", files[0].text);
    }

    #[cfg(unix)]
    #[test]
    fn test_pack_manifest_entry_with_same_size_and_mtime_copy_returns_extracted() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("a.txt"), b"scanned\n");
        let files = pack_after_swap(dir.path(), "a.txt", |path| {
            let copy = path.with_extension("copy");
            fs::copy(path, &copy).unwrap();
            copy_mtime(path, &copy);
            fs::rename(&copy, path).unwrap();
        });
        assert_eq!(files[0].status, FileStatus::Extracted, "{:?}", files[0]);
        assert_eq!(files[0].text, "scanned\n");
    }

    #[cfg(unix)]
    #[test]
    fn test_pack_manifest_entry_with_folder_swapped_for_symlink_returns_changed() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        write(&outside.path().join("b.txt"), b"outside\n");
        write(&dir.path().join("sub/b.txt"), b"inside!\n");
        // The same name, size, and time: only the path to it tells them apart.
        copy_mtime(&outside.path().join("b.txt"), &dir.path().join("sub/b.txt"));
        let files = pack_after_swap(dir.path(), "sub/b.txt", |path| {
            let sub = path.parent().unwrap();
            fs::rename(sub, sub.with_file_name("sub.old")).unwrap();
            std::os::unix::fs::symlink(outside.path(), sub).unwrap();
        });
        assert_eq!(files[0].status, FileStatus::Changed, "{:?}", files[0]);
        assert!(
            files[0].text.contains("replaced by a symlink"),
            "{}",
            files[0].text
        );
        assert!(!files[0].text.contains("outside"), "{}", files[0].text);
    }

    #[cfg(unix)]
    #[test]
    fn test_pack_manifest_entry_with_folder_gone_returns_changed() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("sub/b.txt"), b"inside!\n");
        let files = pack_after_swap(dir.path(), "sub/b.txt", |path| {
            fs::remove_dir_all(path.parent().unwrap()).unwrap();
        });
        assert_eq!(files[0].status, FileStatus::Changed, "{:?}", files[0]);
        assert!(
            files[0].text.contains("no longer there"),
            "{}",
            files[0].text
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_pack_with_follow_links_reads_file_in_symlinked_folder() {
        let dir = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        write(&elsewhere.path().join("b.txt"), b"linked in\n");
        std::os::unix::fs::symlink(elsewhere.path(), dir.path().join("sub")).unwrap();
        let packed = pack(&Options {
            roots: vec![dir.path().to_path_buf()],
            follow_links: true,
            ..Options::default()
        })
        .unwrap();
        let file = packed
            .files
            .iter()
            .find(|f| f.relative == "sub/b.txt")
            .unwrap();
        assert_eq!(file.status, FileStatus::Extracted, "{file:?}");
        assert_eq!(file.text, "linked in\n");
    }

    #[test]
    fn test_pack_entries_with_repeated_tar_member_returns_distinct_ids() {
        let mut builder = tar::Builder::new(Vec::new());
        for body in [&b"first\n"[..], &b"second\n"[..]] {
            let mut header = tar::Header::new_gnu();
            header.set_size(body.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append_data(&mut header, "same.txt", body).unwrap();
        }
        let tar = builder.into_inner().unwrap();
        let entries = [MemoryFile {
            id: "logs.tar",
            relative: "logs.tar",
            bytes: &tar,
        }];
        let opts = Options {
            follow_archives: true,
            ..Options::default()
        };
        let packed = pack_entries(&entries, &opts, None).unwrap();
        let ids: Vec<&str> = packed.files.iter().map(|f| f.id.as_str()).collect();
        assert_eq!(
            ids,
            ["logs.tar!logs.tar/same.txt", "logs.tar!logs.tar/same.txt#2"]
        );
        assert_eq!(packed.files[0].text, "first\n");
        assert_eq!(packed.files[1].text, "second\n");
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

    fn deflated_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        use std::io::{Cursor, Write as IoWrite};
        let mut zw = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let opt = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        for (name, data) in entries {
            zw.start_file(*name, opt).unwrap();
            zw.write_all(data).unwrap();
        }
        zw.finish().unwrap().into_inner()
    }

    /// Offset of the local header that names `name` in `zip`.
    fn local_header_of(zip: &[u8], name: &str) -> usize {
        let at = zip
            .windows(name.len())
            .position(|window| window == name.as_bytes())
            .unwrap();
        at - 30
    }

    fn follow_archives() -> Options {
        Options {
            follow_archives: true,
            ..Options::default()
        }
    }

    #[test]
    fn test_process_item_with_excluded_members_first_keeps_later_included_member() {
        // `*.bin` is excluded by default, so reading it first would spend the
        // budget on bytes that are thrown away.
        let zip = stored_zip(&[("model.bin", &[b'b'; 600]), ("notes.txt", &[b'n'; 600])]);
        let opts = follow_archives();
        let mut budget = ArchiveBudget::new(1000, 1 << 30);
        let files = process_item(
            mem_item("bundle.zip", zip),
            &opts,
            &ExtractOpts::from_options(&opts),
            &mut budget,
        );
        let rels: Vec<&str> = files.iter().map(|f| f.relative.as_str()).collect();
        assert_eq!(rels, ["bundle.zip/notes.txt"]);
        assert!(!budget.cut());
    }

    #[test]
    fn test_process_item_with_binary_member_names_it_without_reading_it() {
        let zip = stored_zip(&[("photo.png", &[7u8; 600]), ("notes.txt", &[b'n'; 600])]);
        let opts = follow_archives();
        let mut budget = ArchiveBudget::new(1000, 1 << 30);
        let files = process_item(
            mem_item("bundle.zip", zip),
            &opts,
            &ExtractOpts::from_options(&opts),
            &mut budget,
        );
        assert_eq!(files.len(), 2, "{files:?}");
        assert_eq!(files[0].relative, "bundle.zip/photo.png");
        assert_eq!(files[0].status, FileStatus::SkippedBinary);
        assert_eq!(files[0].text, "[binary file, 600 bytes]");
        assert_eq!(files[0].size, 600);
        assert_eq!(files[1].status, FileStatus::Extracted);
        assert!(!budget.cut());
    }

    #[test]
    fn test_process_item_with_corrupt_and_unsupported_zip_members_notes_them() {
        let mut zip = deflated_zip(&[
            ("a_good.txt", &[b'a'; 4000]),
            ("b_bad.txt", &[b'b'; 4000]),
            ("c_bzip2.txt", &[b'c'; 4000]),
            ("d_good.txt", &[b'd'; 4000]),
        ]);
        let bad = local_header_of(&zip, "b_bad.txt");
        let extra = usize::from(u16::from_le_bytes([zip[bad + 28], zip[bad + 29]]));
        let data = bad + 30 + "b_bad.txt".len() + extra;
        for byte in &mut zip[data..data + 6] {
            *byte ^= 0xFF;
        }
        // Method 12 is bzip2, which this build cannot read. The central
        // directory entry comes after every local header.
        let local = local_header_of(&zip, "c_bzip2.txt");
        let central = zip
            .windows("c_bzip2.txt".len())
            .rposition(|window| window == b"c_bzip2.txt")
            .unwrap()
            - 46;
        for at in [local + 8, central + 10] {
            zip[at..at + 2].copy_from_slice(&12u16.to_le_bytes());
        }
        let opts = follow_archives();
        let files = process_item(
            mem_item("bundle.zip", zip),
            &opts,
            &ExtractOpts::from_options(&opts),
            &mut ArchiveBudget::default(),
        );
        let status: Vec<(&str, &str)> = files
            .iter()
            .map(|f| (f.relative.as_str(), f.status.as_str()))
            .collect();
        assert_eq!(
            status,
            [
                ("bundle.zip/a_good.txt", "extracted"),
                ("bundle.zip/b_bad.txt", "unreadable"),
                ("bundle.zip/c_bzip2.txt", "unreadable"),
                ("bundle.zip/d_good.txt", "extracted"),
            ]
        );
        assert!(
            files[1].text.starts_with("[text unreadable: "),
            "{}",
            files[1].text
        );
        assert!(files[2].text.contains("ompression"), "{}", files[2].text);
        assert_eq!(files[3].text, "d".repeat(4000));
    }

    #[test]
    fn test_process_item_with_nested_archives_shares_one_budget() {
        let inner = stored_zip(&[("x.txt", &[b'x'; 1000])]);
        let outer = stored_zip(&[("one.zip", &inner), ("two.zip", &inner)]);
        let opts = Options {
            follow_archives: true,
            ..Options::default()
        };
        let extract_opts = ExtractOpts::from_options(&opts);
        let copied_inner = 2 * inner.len() as u64;
        let mut budget = ArchiveBudget::new(copied_inner + 1500, 1 << 30);
        let files = process_item(
            mem_item("outer.zip", outer),
            &opts,
            &extract_opts,
            &mut budget,
        );
        let rels: Vec<&str> = files.iter().map(|f| f.relative.as_str()).collect();
        assert_eq!(rels, ["outer.zip/one.zip/x.txt"], "{rels:?}");
        assert!(budget.cut());
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

    #[test]
    fn test_pack_entries_with_corrupt_zip_returns_unreadable_note() {
        let entries = [MemoryFile {
            id: "bundle.zip",
            relative: "bundle.zip",
            bytes: b"PK\x03\x04 truncated",
        }];
        let opts = Options {
            follow_archives: true,
            ..Options::default()
        };
        let packed = pack_entries(&entries, &opts, None).unwrap();
        assert_eq!(packed.files.len(), 1);
        let file = &packed.files[0];
        assert!(
            matches!(file.status, FileStatus::Unreadable(_)),
            "{:?}",
            file.status
        );
        assert!(file.text.starts_with("[zip unreadable: "), "{}", file.text);
    }

    #[test]
    fn test_pack_entries_with_archive_past_depth_limit_says_why_it_stays_packed() {
        let mut nested = stored_zip(&[("deep.txt", b"deepest")]);
        for level in (1..=3).rev() {
            let name = format!("l{level}.zip");
            nested = stored_zip(&[(name.as_str(), &nested)]);
        }
        let entries = [MemoryFile {
            id: "top.zip",
            relative: "top.zip",
            bytes: &nested,
        }];
        let opts = Options {
            follow_archives: true,
            ..Options::default()
        };
        let packed = pack_entries(&entries, &opts, None).unwrap();
        let skipped: Vec<&PackedFile> = packed
            .files
            .iter()
            .filter(|f| f.status == FileStatus::SkippedArchive)
            .collect();
        assert_eq!(skipped.len(), 1, "{:?}", packed.files);
        assert!(
            skipped[0].text.contains("nested more than 3 archives deep"),
            "{}",
            skipped[0].text
        );
        assert!(!skipped[0].text.contains("pass --archives"));
    }

    #[test]
    fn test_pack_entries_with_oversized_archive_member_returns_too_large_entry() {
        use std::io::{Cursor, Write as IoWrite};
        let mut zw = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let opt = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        zw.start_file("big.log", opt).unwrap();
        zw.write_all(&[b'x'; 64 * 1024]).unwrap();
        zw.start_file("small.txt", opt).unwrap();
        zw.write_all(b"ok\n").unwrap();
        let zip = zw.finish().unwrap().into_inner();
        assert!(zip.len() < 1024, "{} bytes", zip.len());
        let entries = [MemoryFile {
            id: "logs.zip",
            relative: "logs.zip",
            bytes: &zip,
        }];
        let opts = Options {
            follow_archives: true,
            max_file_size: 1024,
            tree: TreeMode::Full,
            ..Options::default()
        };
        let packed = pack_entries(&entries, &opts, None).unwrap();
        let rels: Vec<&str> = packed.files.iter().map(|f| f.relative.as_str()).collect();
        assert_eq!(rels, ["logs.zip/big.log", "logs.zip/small.txt"]);
        let big = &packed.files[0];
        assert_eq!(big.status, FileStatus::TooLarge(1024));
        assert_eq!(big.size, 64 * 1024);
        assert!(packed.tree.contains("big.log"), "{}", packed.tree);
    }
}
