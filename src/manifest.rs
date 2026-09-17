//! Shared discovery result for scan, tree, and pack.

use std::path::PathBuf;

use crate::classify::{Kind, classify, is_default_selected, language_label};
use crate::config::Options;
use crate::error::Error;
use crate::walk::{self, WalkedFile};

/// One discovered file. `id` is the stable key used for selection.
#[derive(Debug, Clone)]
pub struct ManifestEntry {
    pub id: String,
    pub relative: String,
    pub absolute: PathBuf,
    pub size: u64,
    pub kind: Kind,
    pub language: &'static str,
    pub default_on: bool,
    pub oversized: bool,
    pub is_symlink: bool,
}

/// Files found under the current options, before extraction.
#[derive(Debug, Clone)]
pub struct ScanManifest {
    pub root: PathBuf,
    pub entries: Vec<ManifestEntry>,
    pub bytes: u64,
    pub truncated: bool,
}

/// Walk roots with `opts` and record every eligible entry once.
pub fn scan_manifest(opts: &Options) -> Result<ScanManifest, Error> {
    let walked = walk::collect(opts)?;
    let (entries, truncated) = entries_from_walked(walked, opts);
    let bytes = entries.iter().map(|e| e.size).sum();
    let root = opts
        .roots
        .first()
        .cloned()
        .unwrap_or_else(|| PathBuf::from("."));
    Ok(ScanManifest {
        root,
        entries,
        bytes,
        truncated,
    })
}

fn entries_from_walked(mut walked: Vec<WalkedFile>, opts: &Options) -> (Vec<ManifestEntry>, bool) {
    let mut truncated = false;
    if walked.len() > opts.max_entries {
        walked.truncate(opts.max_entries);
        truncated = true;
    }
    let mut total = 0u64;
    let mut entries = Vec::with_capacity(walked.len());
    for wf in walked {
        if total.saturating_add(wf.size) > opts.max_total_bytes {
            truncated = true;
            continue;
        }
        total = total.saturating_add(wf.size);
        let kind = classify(&wf.absolute, None);
        let oversized = wf.size > opts.max_file_size;
        entries.push(ManifestEntry {
            id: wf.relative.clone(),
            relative: wf.relative.clone(),
            language: language_label(&wf.absolute),
            default_on: is_default_selected(&wf.absolute, kind) && !oversized,
            oversized,
            is_symlink: wf.is_symlink,
            absolute: wf.absolute,
            size: wf.size,
            kind,
        });
    }
    (entries, truncated)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;

    use crate::config::Selection;

    fn write(path: &Path, body: &[u8]) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, body).unwrap();
    }

    #[test]
    fn test_scan_manifest_with_max_entries_sets_truncated() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("a.rs"), b"fn a() {}\n");
        write(&dir.path().join("b.rs"), b"fn b() {}\n");
        let opts = Options {
            roots: vec![dir.path().to_path_buf()],
            max_entries: 1,
            ..Options::default()
        };
        let manifest = scan_manifest(&opts).unwrap();
        assert_eq!(manifest.entries.len(), 1);
        assert!(manifest.truncated);
    }

    #[test]
    fn test_scan_manifest_with_max_total_bytes_sets_truncated() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("a.rs"), b"aaaa\n");
        write(&dir.path().join("b.rs"), b"bbbb\n");
        let opts = Options {
            roots: vec![dir.path().to_path_buf()],
            max_total_bytes: 5,
            ..Options::default()
        };
        let manifest = scan_manifest(&opts).unwrap();
        assert!(manifest.entries.len() < 2);
        assert!(manifest.truncated);
    }

    #[test]
    fn test_scan_manifest_with_oversized_file_sets_default_on_false() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("tiny.rs"), b"fn t() {}\n");
        write(&dir.path().join("huge.rs"), &[b'x'; 64]);
        let opts = Options {
            roots: vec![dir.path().to_path_buf()],
            max_file_size: 16,
            ..Options::default()
        };
        let manifest = scan_manifest(&opts).unwrap();
        let huge = manifest
            .entries
            .iter()
            .find(|e| e.relative == "huge.rs")
            .unwrap();
        assert!(huge.oversized);
        assert!(!huge.default_on);
        let tiny = manifest
            .entries
            .iter()
            .find(|e| e.relative == "tiny.rs")
            .unwrap();
        assert!(!tiny.oversized);
        assert!(tiny.default_on);
    }

    #[test]
    fn test_scan_manifest_with_empty_only_returns_no_entries() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("a.rs"), b"fn a() {}\n");
        let opts = Options {
            roots: vec![dir.path().to_path_buf()],
            selection: Selection::Only(Vec::new()),
            ..Options::default()
        };
        let manifest = scan_manifest(&opts).unwrap();
        assert!(manifest.entries.is_empty());
        assert!(!manifest.truncated);
    }
}
