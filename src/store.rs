//! Bounded mill session: manifests, extraction results, the extraction cache
//! of the last pack, one pack job, and one preview.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use crate::cache::ExtractCache;
use crate::manifest::{ManifestEntry, ScanManifest};
use crate::pack::Packed;

const MAX_ITEMS: usize = 8;
/// Extracted text kept for redraws and full downloads. The newest result stays
/// even when it alone is larger, so its dump can still be copied or saved.
const MAX_RESULT_BYTES: usize = 64 * 1024 * 1024;
/// Scanned file lists kept for packs and previews, estimated by
/// [`manifest_bytes`]. The newest manifest always stays.
const MAX_MANIFEST_BYTES: usize = 64 * 1024 * 1024;
pub const PREVIEW_CHARS: usize = 32 * 1024;

/// In-memory mill session.
pub struct Mill {
    busy: AtomicBool,
    /// Separate from `busy` so a preview and a pack never turn each other away.
    preview_busy: AtomicBool,
    job_seq: AtomicU64,
    current: Mutex<Option<Arc<PackJob>>>,
    manifests: Mutex<Lru<Arc<StoredManifest>>>,
    results: Mutex<Lru<Arc<StoredResult>>>,
    /// What the most recent pack extracted, for the next pack to reuse. It
    /// shares that pack's files with its stored result, so it costs no
    /// second copy of their text.
    extract_cache: Mutex<Option<Arc<ExtractCache>>>,
}

/// One admitted pack job with its own cancel flag and progress.
pub struct PackJob {
    pub id: u64,
    pub cancel: AtomicBool,
    /// Selected entries this job has finished extracting.
    pub done: AtomicUsize,
    /// Selected entries this job extracts. Zero until extraction starts.
    pub total: AtomicUsize,
}

/// Discovery snapshot keyed by [`StoredManifest::id`].
pub struct StoredManifest {
    pub id: String,
    pub discovery_key: String,
    pub manifest: ScanManifest,
}

/// Extracted files keyed by [`StoredResult::id`]. Re-render without re-extract.
pub struct StoredResult {
    pub id: String,
    pub manifest_id: String,
    /// The pack's files and totals, shared with the extraction cache made
    /// from the same pack.
    pub packed: Arc<Packed>,
    pub roots: Vec<PathBuf>,
    pub source_mode: bool,
}

/// Least recently used store with an item cap and a byte cap.
struct Lru<T> {
    /// Oldest first; the back is the most recently stored or read.
    order: VecDeque<String>,
    /// Each item with the bytes it was stored with.
    items: HashMap<String, (T, usize)>,
    /// Sum of the byte counts in `items`.
    bytes: usize,
    max_bytes: usize,
}

impl<T> Lru<T> {
    fn new(max_bytes: usize) -> Self {
        Self {
            order: VecDeque::new(),
            items: HashMap::new(),
            bytes: 0,
            max_bytes,
        }
    }
}

impl Default for Mill {
    fn default() -> Self {
        Self {
            busy: AtomicBool::new(false),
            preview_busy: AtomicBool::new(false),
            job_seq: AtomicU64::new(1),
            current: Mutex::new(None),
            manifests: Mutex::new(Lru::new(MAX_MANIFEST_BYTES)),
            results: Mutex::new(Lru::new(MAX_RESULT_BYTES)),
            extract_cache: Mutex::new(None),
        }
    }
}

/// Rough heap and inline size of a scanned file list, for the manifest cap.
fn manifest_bytes(manifest: &ScanManifest) -> usize {
    manifest
        .entries
        .iter()
        .map(|entry| {
            std::mem::size_of::<ManifestEntry>()
                + entry.id.len()
                + entry.relative.len()
                + entry.absolute.as_os_str().len()
                + entry.language.len()
        })
        .sum()
}

impl Mill {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Admit one pack job. Does not clear another job's cancel flag.
    #[must_use]
    pub fn try_begin_pack(&self) -> Option<Arc<PackJob>> {
        if self
            .busy
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return None;
        }
        let job = Arc::new(PackJob {
            id: self.job_seq.fetch_add(1, Ordering::SeqCst),
            cancel: AtomicBool::new(false),
            done: AtomicUsize::new(0),
            total: AtomicUsize::new(0),
        });
        *self.current.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::clone(&job));
        Some(job)
    }

    pub fn end_pack(&self, job: &PackJob) {
        let mut current = self.current.lock().unwrap_or_else(|e| e.into_inner());
        if current.as_ref().is_some_and(|cur| cur.id == job.id) {
            *current = None;
            self.busy.store(false, Ordering::SeqCst);
        }
    }

    pub fn request_cancel(&self) {
        if let Some(job) = self
            .current
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            job.cancel.store(true, Ordering::SeqCst);
        }
    }

    pub fn current_job(&self) -> Option<Arc<PackJob>> {
        self.current
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// `(done, total)` entries of the running pack job; `None` while idle.
    pub fn pack_progress(&self) -> Option<(usize, usize)> {
        let job = self.current_job()?;
        // `total` is set before the first entry finishes, so reading `done`
        // first never yields more done than total.
        let done = job.done.load(Ordering::SeqCst);
        Some((done, job.total.load(Ordering::SeqCst)))
    }

    /// Admit one preview. Previews have their own slot, so one runs beside a
    /// pack; only a second preview is turned away.
    #[must_use]
    pub fn try_begin_preview(self: &Arc<Self>) -> Option<PreviewGuard> {
        self.preview_busy
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .ok()?;
        Some(PreviewGuard {
            mill: Arc::clone(self),
        })
    }

    /// Store a scan under a fresh id and hand it back, so the caller can use
    /// it without cloning the file list.
    pub fn put_manifest(
        &self,
        discovery_key: String,
        manifest: ScanManifest,
    ) -> Arc<StoredManifest> {
        let id = new_id();
        let bytes = manifest_bytes(&manifest);
        let stored = Arc::new(StoredManifest {
            id: id.clone(),
            discovery_key,
            manifest,
        });
        self.manifests
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(id, Arc::clone(&stored), bytes);
        stored
    }

    pub fn get_manifest(&self, id: &str) -> Option<Arc<StoredManifest>> {
        self.manifests
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
            .cloned()
    }

    pub fn put_result(&self, result: StoredResult) -> Arc<StoredResult> {
        let bytes = result
            .packed
            .files
            .iter()
            .map(|f| f.text.len())
            .sum::<usize>();
        let id = result.id.clone();
        let cancelled = result.packed.stats.cancelled;
        let arc = Arc::new(result);
        if !cancelled {
            self.results.lock().unwrap_or_else(|e| e.into_inner()).push(
                id,
                Arc::clone(&arc),
                bytes,
            );
        }
        arc
    }

    pub fn get_result(&self, id: &str) -> Option<Arc<StoredResult>> {
        self.results
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
            .cloned()
    }

    /// The extraction cache of the most recent pack.
    pub fn extract_cache(&self) -> Option<Arc<ExtractCache>> {
        self.extract_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Keep `cache` for the next pack, in place of the one before.
    ///
    /// Only one is kept, and it holds the files of the newest pack, which
    /// the result store keeps whatever its size, so the cache adds nothing
    /// to the text the mill holds. A cancelled pack's result is not stored,
    /// so its cache alone holds what that pack read before it stopped.
    pub fn keep_extract_cache(&self, cache: ExtractCache) {
        let previous = self
            .extract_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .replace(Arc::new(cache));
        // Freed outside the lock.
        drop(previous);
    }
}

impl<T> Lru<T> {
    /// Store `value` as the most recent item, then drop the least recent ones
    /// until the store is within its item and byte caps. The item just stored
    /// always stays, even when it alone is over the byte cap.
    fn push(&mut self, id: String, value: T, bytes: usize) {
        if let Some((_, replaced)) = self.items.insert(id.clone(), (value, bytes)) {
            self.bytes = self.bytes.saturating_sub(replaced);
            self.order.retain(|k| *k != id);
        }
        self.order.push_back(id);
        self.bytes = self.bytes.saturating_add(bytes);
        while self.order.len() > 1 && (self.order.len() > MAX_ITEMS || self.bytes > self.max_bytes)
        {
            let Some(old) = self.order.pop_front() else {
                break;
            };
            if let Some((_, freed)) = self.items.remove(&old) {
                self.bytes = self.bytes.saturating_sub(freed);
            }
        }
    }

    fn get(&mut self, id: &str) -> Option<&T> {
        if self.items.contains_key(id) {
            self.order.retain(|k| k != id);
            self.order.push_back(id.to_string());
            self.items.get(id).map(|(value, _)| value)
        } else {
            None
        }
    }
}

/// Drops the busy flag when the worker finishes, even if the HTTP task is gone.
pub struct JobGuard {
    pub mill: Arc<Mill>,
    pub job: Arc<PackJob>,
}

impl Drop for JobGuard {
    fn drop(&mut self) {
        self.mill.end_pack(&self.job);
    }
}

/// Holds the preview slot. Move it into the worker so the slot frees when the
/// worker finishes, even if the HTTP task is gone.
pub struct PreviewGuard {
    mill: Arc<Mill>,
}

impl Drop for PreviewGuard {
    fn drop(&mut self) {
        self.mill.preview_busy.store(false, Ordering::SeqCst);
    }
}

pub fn new_id() -> String {
    let mut buf = [0u8; 8];
    let _ = getrandom::fill(&mut buf);
    buf.iter().fold(String::with_capacity(16), |mut s, b| {
        s.push_str(&format!("{b:02x}"));
        s
    })
}

pub fn cap_preview(dump: &str) -> (String, bool) {
    if dump.len() <= PREVIEW_CHARS {
        return (dump.to_string(), false);
    }
    let mut end = PREVIEW_CHARS;
    while end > 0 && !dump.is_char_boundary(end) {
        end -= 1;
    }
    (dump[..end].to_string(), true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_try_begin_pack_with_busy_returns_none() {
        let mill = Mill::new();
        let job = mill.try_begin_pack().expect("first");
        assert!(mill.try_begin_pack().is_none());
        mill.request_cancel();
        assert!(job.cancel.load(Ordering::SeqCst));
        mill.end_pack(&job);
        assert!(mill.try_begin_pack().is_some());
    }

    #[test]
    fn test_try_begin_pack_does_not_clear_other_cancel() {
        let mill = Mill::new();
        let job = mill.try_begin_pack().expect("first");
        mill.request_cancel();
        assert!(mill.try_begin_pack().is_none());
        assert!(job.cancel.load(Ordering::SeqCst));
        mill.end_pack(&job);
    }

    #[test]
    fn test_pack_progress_with_running_job_returns_done_and_total() {
        let mill = Mill::new();
        assert_eq!(mill.pack_progress(), None);
        let job = mill.try_begin_pack().expect("first");
        assert_eq!(mill.pack_progress(), Some((0, 0)));
        job.total.store(41, Ordering::SeqCst);
        job.done.store(23, Ordering::SeqCst);
        assert_eq!(mill.pack_progress(), Some((23, 41)));
        mill.end_pack(&job);
        assert_eq!(mill.pack_progress(), None);
        let next = mill.try_begin_pack().expect("second");
        assert_eq!(
            mill.pack_progress(),
            Some((0, 0)),
            "a new job starts at zero"
        );
        mill.end_pack(&next);
    }

    #[test]
    fn test_try_begin_preview_with_busy_returns_none() {
        let mill = Arc::new(Mill::new());
        let slot = mill.try_begin_preview().expect("first");
        assert!(mill.try_begin_preview().is_none());
        drop(slot);
        assert!(mill.try_begin_preview().is_some());
    }

    #[test]
    fn test_try_begin_preview_with_pack_running_returns_slot() {
        let mill = Arc::new(Mill::new());
        let job = mill.try_begin_pack().expect("pack");
        let slot = mill.try_begin_preview().expect("preview beside a pack");
        mill.end_pack(&job);
        assert!(mill.try_begin_pack().is_some(), "pack beside a preview");
        drop(slot);
    }

    /// Byte counts the store holds for every retained item, summed.
    fn held_bytes<T>(lru: &Lru<T>) -> usize {
        lru.items.values().map(|(_, bytes)| bytes).sum()
    }

    #[test]
    fn test_lru_push_with_small_then_large_items_keeps_bytes_within_cap() {
        let mut lru: Lru<()> = Lru::new(64);
        for n in 0..7 {
            lru.push(format!("small{n}"), (), 1);
        }
        for n in 0..3 {
            lru.push(format!("large{n}"), (), 60);
        }
        assert_eq!(lru.bytes, held_bytes(&lru), "tracked bytes drifted");
        assert!(lru.bytes <= 64, "store holds {} bytes", lru.bytes);
        assert_eq!(lru.order.len(), lru.items.len());
    }

    #[test]
    fn test_lru_push_with_item_over_cap_keeps_newest() {
        let mut lru: Lru<()> = Lru::new(64);
        lru.push("old".into(), (), 10);
        lru.push("huge".into(), (), 100);
        assert!(lru.get("huge").is_some(), "the newest item was evicted");
        assert!(lru.get("old").is_none());
        assert_eq!(lru.bytes, 100);
        lru.push("next".into(), (), 1);
        assert!(lru.get("huge").is_none());
        assert_eq!(lru.bytes, 1);
    }

    #[test]
    fn test_lru_push_with_more_than_max_items_drops_least_recent() {
        let mut lru: Lru<()> = Lru::new(usize::MAX);
        for n in 0..MAX_ITEMS {
            lru.push(format!("item{n}"), (), 1);
        }
        // Reading item0 makes item1 the least recently used.
        assert!(lru.get("item0").is_some());
        lru.push("fresh".into(), (), 1);
        assert!(lru.get("item0").is_some());
        assert!(lru.get("item1").is_none());
        assert_eq!(lru.items.len(), MAX_ITEMS);
        assert_eq!(lru.bytes, MAX_ITEMS);
    }

    fn result_of(text_bytes: usize) -> StoredResult {
        StoredResult {
            id: new_id(),
            manifest_id: "m".into(),
            packed: Arc::new(Packed {
                files: vec![crate::pack::PackedFile {
                    id: "a.txt".into(),
                    relative: "a.txt".into(),
                    kind: crate::classify::Kind::Text,
                    size: text_bytes as u64,
                    text: "x".repeat(text_bytes),
                    status: crate::pack::FileStatus::Extracted,
                }],
                tree: String::new(),
                stats: crate::pack::Stats::default(),
            }),
            roots: Vec::new(),
            source_mode: false,
        }
    }

    #[test]
    fn test_put_result_with_result_over_cap_keeps_it_for_download() {
        let mill = Mill::new();
        let stored = mill.put_result(result_of(MAX_RESULT_BYTES + 1));
        assert!(
            mill.get_result(&stored.id).is_some(),
            "a dump over the cap must stay for Copy and Save"
        );
    }

    #[test]
    fn test_put_manifest_with_many_entries_counts_their_bytes() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn a() {}\n").unwrap();
        let scanned = crate::manifest::scan_manifest(&crate::config::Options {
            roots: vec![dir.path().to_path_buf()],
            list_only: true,
            ..crate::config::Options::default()
        })
        .unwrap();
        let manifest = ScanManifest {
            entries: vec![scanned.entries[0].clone(); 1000],
            ..scanned
        };
        let expected = manifest_bytes(&manifest);
        assert!(expected >= 1000 * std::mem::size_of::<ManifestEntry>());
        let mill = Mill::new();
        let stored = mill.put_manifest("key".into(), manifest);
        assert_eq!(stored.manifest.entries.len(), 1000);
        let held = mill.manifests.lock().unwrap().bytes;
        assert_eq!(held, expected);
    }

    #[test]
    fn test_cap_preview_with_short_dump_returns_full() {
        let (text, truncated) = cap_preview("hello");
        assert_eq!(text, "hello");
        assert!(!truncated);
    }
}
