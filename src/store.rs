//! Bounded mill session: manifests, extraction results, and one pack job.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::manifest::ScanManifest;
use crate::pack::{PackedFile, Stats};

const MAX_ITEMS: usize = 8;
const MAX_RESULT_BYTES: usize = 64 * 1024 * 1024;
pub const PREVIEW_CHARS: usize = 32 * 1024;

/// In-memory mill session.
pub struct Mill {
    busy: AtomicBool,
    job_seq: AtomicU64,
    current: Mutex<Option<Arc<PackJob>>>,
    manifests: Mutex<Lru<StoredManifest>>,
    results: Mutex<Lru<Arc<StoredResult>>>,
}

/// One admitted pack/preview job with its own cancel flag.
pub struct PackJob {
    pub id: u64,
    pub cancel: AtomicBool,
}

/// Discovery snapshot keyed by [`StoredManifest::id`].
#[derive(Clone)]
pub struct StoredManifest {
    pub id: String,
    pub discovery_key: String,
    pub manifest: ScanManifest,
}

/// Extracted files keyed by [`StoredResult::id`]. Re-render without re-extract.
pub struct StoredResult {
    pub id: String,
    pub manifest_id: String,
    pub extract_key: String,
    pub files: Vec<PackedFile>,
    pub stats: Stats,
    pub roots: Vec<PathBuf>,
    pub source_mode: bool,
}

struct Lru<T> {
    order: VecDeque<String>,
    items: HashMap<String, T>,
    bytes: usize,
}

impl<T> Default for Lru<T> {
    fn default() -> Self {
        Self {
            order: VecDeque::new(),
            items: HashMap::new(),
            bytes: 0,
        }
    }
}

impl Default for Mill {
    fn default() -> Self {
        Self {
            busy: AtomicBool::new(false),
            job_seq: AtomicU64::new(1),
            current: Mutex::new(None),
            manifests: Mutex::new(Lru::default()),
            results: Mutex::new(Lru::default()),
        }
    }
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

    pub fn put_manifest(&self, discovery_key: String, manifest: ScanManifest) -> String {
        let id = new_id();
        let stored = StoredManifest {
            id: id.clone(),
            discovery_key,
            manifest,
        };
        self.manifests
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(id.clone(), stored, 0);
        id
    }

    pub fn get_manifest(&self, id: &str) -> Option<StoredManifest> {
        self.manifests
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
            .cloned()
    }

    pub fn put_result(&self, result: StoredResult) -> Arc<StoredResult> {
        let bytes = result.files.iter().map(|f| f.text.len()).sum::<usize>();
        let id = result.id.clone();
        let cancelled = result.stats.cancelled;
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

    pub fn find_result(&self, manifest_id: &str, extract_key: &str) -> Option<Arc<StoredResult>> {
        let mut guard = self.results.lock().unwrap_or_else(|e| e.into_inner());
        let id = guard.items.iter().find_map(|(id, r)| {
            if r.manifest_id == manifest_id && r.extract_key == extract_key && !r.stats.cancelled {
                Some(id.clone())
            } else {
                None
            }
        })?;
        guard.get(&id).cloned()
    }
}

impl<T> Lru<T> {
    fn push(&mut self, id: String, value: T, add_bytes: usize) {
        if self.items.insert(id.clone(), value).is_none() {
            self.order.push_back(id);
        }
        self.bytes = self.bytes.saturating_add(add_bytes);
        while self.order.len() > MAX_ITEMS || self.bytes > MAX_RESULT_BYTES {
            if let Some(old) = self.order.pop_front() {
                self.items.remove(&old);
                self.bytes = self.bytes.saturating_div(2);
            } else {
                break;
            }
        }
    }

    fn get(&mut self, id: &str) -> Option<&T> {
        if self.items.contains_key(id) {
            self.order.retain(|k| k != id);
            self.order.push_back(id.to_string());
            self.items.get(id)
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
    fn test_cap_preview_with_short_dump_returns_full() {
        let (text, truncated) = cap_preview("hello");
        assert_eq!(text, "hello");
        assert!(!truncated);
    }
}
