//! Bounded mill session: manifests, extraction results, and one pack job.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::manifest::ScanManifest;
use crate::pack::{PackedFile, Stats};

const MAX_ITEMS: usize = 8;
pub const PREVIEW_CHARS: usize = 32 * 1024;

/// In-memory mill session.
pub struct Mill {
    pub busy: AtomicBool,
    pub cancel: AtomicBool,
    manifests: Mutex<Lru<StoredManifest>>,
    results: Mutex<Lru<StoredResult>>,
}

/// Discovery snapshot keyed by [`StoredManifest::id`].
#[derive(Clone)]
pub struct StoredManifest {
    pub id: String,
    #[allow(dead_code)]
    pub discovery_key: String,
    pub manifest: ScanManifest,
}

/// Extracted files keyed by [`StoredResult::id`]. Re-render without re-extract.
#[derive(Clone)]
pub struct StoredResult {
    pub id: String,
    pub manifest_id: String,
    pub extract_key: String,
    pub files: Vec<PackedFile>,
    pub stats: Stats,
}

struct Lru<T> {
    order: VecDeque<String>,
    items: HashMap<String, T>,
}

impl<T> Default for Lru<T> {
    fn default() -> Self {
        Self {
            order: VecDeque::new(),
            items: HashMap::new(),
        }
    }
}

impl Default for Mill {
    fn default() -> Self {
        Self {
            busy: AtomicBool::new(false),
            cancel: AtomicBool::new(false),
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

    #[must_use]
    pub fn try_begin_pack(&self) -> bool {
        self.cancel.store(false, Ordering::SeqCst);
        self.busy
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }

    pub fn end_pack(&self) {
        self.busy.store(false, Ordering::SeqCst);
        self.cancel.store(false, Ordering::SeqCst);
    }

    pub fn request_cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
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
            .push(id.clone(), stored);
        id
    }

    pub fn get_manifest(&self, id: &str) -> Option<StoredManifest> {
        self.manifests
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
            .cloned()
    }

    pub fn put_result(&self, result: StoredResult) -> String {
        let id = result.id.clone();
        self.results
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(id.clone(), result);
        id
    }

    pub fn get_result(&self, id: &str) -> Option<StoredResult> {
        self.results
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
            .cloned()
    }

    pub fn find_result(&self, manifest_id: &str, extract_key: &str) -> Option<StoredResult> {
        let guard = self.results.lock().unwrap_or_else(|e| e.into_inner());
        guard
            .items
            .values()
            .find(|r| r.manifest_id == manifest_id && r.extract_key == extract_key)
            .cloned()
    }
}

impl<T> Lru<T> {
    fn push(&mut self, id: String, value: T) {
        if self.items.insert(id.clone(), value).is_none() {
            self.order.push_back(id);
            while self.order.len() > MAX_ITEMS {
                if let Some(old) = self.order.pop_front() {
                    self.items.remove(&old);
                }
            }
        }
    }

    fn get(&self, id: &str) -> Option<&T> {
        self.items.get(id)
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
    fn test_try_begin_pack_with_busy_returns_false() {
        let mill = Mill::new();
        assert!(mill.try_begin_pack());
        assert!(!mill.try_begin_pack());
        mill.end_pack();
        assert!(mill.try_begin_pack());
        mill.end_pack();
    }

    #[test]
    fn test_cap_preview_with_short_dump_returns_full() {
        let (text, truncated) = cap_preview("hello");
        assert_eq!(text, "hello");
        assert!(!truncated);
    }
}
