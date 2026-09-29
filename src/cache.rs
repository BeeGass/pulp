//! What one pack extracted, kept for the next pack of the same files, so a
//! re-pulp reads and extracts only the files that changed.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use crate::config::Options;
use crate::pack::{Packed, PackedFile};

/// Every setting that decides what extracting a file yields.
///
/// A cache answers only a pack whose settings give the same fingerprint.
/// How the dump is drawn, its format and directory map, is left out: the
/// same files drawn another way need no new extraction.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ExtractFingerprint {
    /// The roots, since a file id names a file under them.
    pub roots: Vec<PathBuf>,
    /// Whether symlinked folders are followed, which decides what a path
    /// opens.
    pub follow_links: bool,
    /// The per-file cap, for files and archive members alike.
    pub max_file_size: u64,
    pub follow_archives: bool,
    pub skip_binaries: bool,
    pub notebook_outputs: bool,
    pub source_mode: bool,
    /// Whether hidden archive members are kept.
    pub hidden: bool,
    /// The globs that judge archive members.
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub default_excludes: bool,
}

impl ExtractFingerprint {
    /// The fingerprint of `opts`.
    #[must_use]
    pub fn of(opts: &Options) -> Self {
        Self {
            roots: opts.roots.clone(),
            follow_links: opts.follow_links,
            max_file_size: opts.max_file_size,
            follow_archives: opts.follow_archives,
            skip_binaries: opts.skip_binaries,
            notebook_outputs: opts.notebook_outputs,
            source_mode: opts.source_mode,
            hidden: opts.hidden,
            include: opts.include.clone(),
            exclude: opts.exclude.clone(),
            default_excludes: opts.default_excludes,
        }
    }
}

/// What one pack extracted, by scanned file, for the next pack of the same
/// files to reuse. `pack_manifest_cached` makes one and takes one.
///
/// It holds the pack itself, shared, and an index into the pack's files,
/// so keeping a cache beside the pack costs no second copy of their text.
#[derive(Debug, Clone)]
pub struct ExtractCache {
    fingerprint: ExtractFingerprint,
    packed: Arc<Packed>,
    entries: HashMap<String, CacheEntry>,
    reused: usize,
}

/// One scanned file in an [`ExtractCache`]: the file as it was read, and
/// which of the pack's files it yielded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheEntry {
    /// The file's size when it was read, in bytes.
    pub size: u64,
    /// The file's modification time when it was read, where known.
    pub modified: Option<SystemTime>,
    path: PathBuf,
    bytes_read: u64,
    archive_cut: bool,
    /// Where the file's outcomes are in the pack's files, in dump order: one
    /// for a file, one per member for an expanded archive.
    positions: Vec<usize>,
}

impl ExtractCache {
    /// The pack this cache was made from.
    #[must_use]
    pub fn packed(&self) -> &Arc<Packed> {
        &self.packed
    }

    /// The settings of the pack this cache was made from.
    #[must_use]
    pub fn fingerprint(&self) -> &ExtractFingerprint {
        &self.fingerprint
    }

    /// Whether a pack with `opts` may reuse this cache.
    #[must_use]
    pub fn is_valid_for(&self, opts: &Options) -> bool {
        self.fingerprint == ExtractFingerprint::of(opts)
    }

    /// Files this cache can answer for.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Files the pack that made this cache took from the cache it was
    /// given, rather than reading them.
    #[must_use]
    pub fn reused(&self) -> usize {
        self.reused
    }

    /// The entry for the file `id`, however the file has changed since.
    #[must_use]
    pub fn entry(&self, id: &str) -> Option<&CacheEntry> {
        self.entries.get(id)
    }

    /// The entry for the file `id` when the file at `path` is unchanged
    /// since it was read: the same path, size, and modification time.
    ///
    /// A file whose modification time is unknown never matches, since its
    /// size alone cannot tell an edit. The device and inode do not count,
    /// as they do not for `make`: a remount, a network share reconnecting
    /// after sleep, or an overlay copy-up gives unchanged files new ones.
    #[must_use]
    pub fn lookup(
        &self,
        id: &str,
        path: &Path,
        size: u64,
        modified: Option<SystemTime>,
    ) -> Option<&CacheEntry> {
        let entry = self.entries.get(id)?;
        let unchanged = entry.path == path
            && entry.size == size
            && modified.is_some()
            && entry.modified == modified;
        unchanged.then_some(entry)
    }

    /// What the file `id` yielded, in dump order: its own outcome, or an
    /// expanded archive's members. Nothing for a file this cache lacks.
    pub fn entry_files<'a>(&'a self, id: &str) -> impl Iterator<Item = &'a PackedFile> + use<'a> {
        self.entries
            .get(id)
            .into_iter()
            .flat_map(|entry| entry.positions.iter())
            .filter_map(|at| self.packed.files.get(*at))
    }
}

impl CacheEntry {
    /// Bytes the pack read for this file, which a pack that reuses it
    /// counts again, so its totals match a pack that reads it.
    #[must_use]
    pub fn bytes_read(&self) -> u64 {
        self.bytes_read
    }

    /// Whether the archive budget cut this archive's members short, which
    /// marks a pack that reuses it truncated too.
    #[must_use]
    pub fn archive_cut(&self) -> bool {
        self.archive_cut
    }
}

#[cfg(feature = "native")]
impl ExtractCache {
    pub(crate) fn new(
        fingerprint: ExtractFingerprint,
        packed: Arc<Packed>,
        entries: HashMap<String, CacheEntry>,
        reused: usize,
    ) -> Self {
        Self {
            fingerprint,
            packed,
            entries,
            reused,
        }
    }
}

#[cfg(feature = "native")]
impl CacheEntry {
    pub(crate) fn new(
        size: u64,
        modified: Option<SystemTime>,
        path: PathBuf,
        bytes_read: u64,
        archive_cut: bool,
        positions: Vec<usize>,
    ) -> Self {
        Self {
            size,
            modified,
            path,
            bytes_read,
            archive_cut,
            positions,
        }
    }
}
