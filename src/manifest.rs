//! Shared discovery result for scan, tree, and pack.

use std::path::PathBuf;
use std::time::SystemTime;

use crate::classify::Kind;
#[cfg(feature = "native")]
use crate::classify::{classify, is_default_selected, kind_from_name, language_name};
#[cfg(feature = "native")]
use crate::config::Options;
#[cfg(feature = "native")]
use crate::error::Error;
#[cfg(feature = "native")]
use crate::walk::{self, WalkedFile};

/// One discovered file. `id` is the stable key used for selection.
#[derive(Debug, Clone)]
pub struct ManifestEntry {
    pub id: String,
    pub relative: String,
    /// Path under its own root, without the multi-root label. Globs and the
    /// hidden rule judge archive members by it, as the walk judges files.
    pub root_relative: String,
    pub absolute: PathBuf,
    pub size: u64,
    pub kind: Kind,
    pub language: String,
    pub default_on: bool,
    pub oversized: bool,
    pub is_symlink: bool,
    pub modified: Option<SystemTime>,
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
#[cfg(feature = "native")]
pub fn scan_manifest(opts: &Options) -> Result<ScanManifest, Error> {
    let walked = walk::collect_detailed(opts)?;
    let entries = entries_from_walked(walked.files, opts);
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
        truncated: walked.truncated,
    })
}

#[cfg(feature = "native")]
fn entries_from_walked(walked: Vec<WalkedFile>, opts: &Options) -> Vec<ManifestEntry> {
    walked
        .into_iter()
        .map(|wf| {
            let sniff = sniff_prefix(&wf.absolute);
            let kind = classify(&wf.absolute, sniff.as_deref());
            let oversized = wf.size > opts.max_file_size;
            // Judge generated and virtualenv paths under the scanned root
            // only: a project that lives in `~/build/app` or `~/venv-work`
            // is not itself generated.
            let default_on = is_default_selected(std::path::Path::new(&wf.root_relative), kind)
                && !oversized
                && (!kind.is_archive() || opts.follow_archives);
            ManifestEntry {
                id: wf.id,
                relative: wf.relative,
                root_relative: wf.root_relative,
                language: language_name(&wf.absolute, kind),
                default_on,
                oversized,
                is_symlink: wf.is_symlink,
                modified: wf.modified,
                absolute: wf.absolute,
                size: wf.size,
                kind,
            }
        })
        .collect()
}

/// Read a short prefix when the name alone does not decide the kind.
#[cfg(feature = "native")]
fn sniff_prefix(path: &std::path::Path) -> Option<Vec<u8>> {
    if kind_from_name(path).is_some() {
        return None;
    }
    let mut file = std::fs::File::open(path).ok()?;
    let mut buf = vec![0u8; 8192];
    use std::io::Read;
    let n = file.read(&mut buf).ok()?;
    buf.truncate(n);
    if buf.is_empty() { None } else { Some(buf) }
}

#[cfg(all(test, feature = "native"))]
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
    fn test_scan_manifest_with_venv_file_sets_default_on_false() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("src/app.py"), b"print(1)\n");
        write(&dir.path().join(".venv/lib/site.py"), b"x = 1\n");
        let opts = Options {
            roots: vec![dir.path().to_path_buf()],
            hidden: true,
            gitignore: false,
            default_excludes: false,
            ..Options::default()
        };
        let manifest = scan_manifest(&opts).unwrap();
        let venv = manifest
            .entries
            .iter()
            .find(|e| e.relative.contains(".venv"))
            .expect("expected .venv file in scan");
        assert!(!venv.default_on);
        let app = manifest
            .entries
            .iter()
            .find(|e| e.relative == "src/app.py")
            .unwrap();
        assert!(app.default_on);
    }

    #[test]
    fn test_scan_manifest_with_generated_dirs_omits_them() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("content/index.math"), b"V(h)=1\n");
        write(&dir.path().join("rates.f90"), b"subroutine r\nend\n");
        write(&dir.path().join(".next/server/page.js"), b"export {}\n");
        write(&dir.path().join("out/index.html"), b"<p>x</p>\n");
        write(
            &dir.path().join("toolchains/sdk/a.f90"),
            b"subroutine a\nend\n",
        );
        let opts = Options {
            roots: vec![dir.path().to_path_buf()],
            hidden: true,
            gitignore: false,
            ..Options::default()
        };
        let manifest = scan_manifest(&opts).unwrap();
        let rels: Vec<&str> = manifest
            .entries
            .iter()
            .map(|e| e.relative.as_str())
            .collect();
        assert!(rels.contains(&"content/index.math"), "{rels:?}");
        assert!(rels.contains(&"rates.f90"), "{rels:?}");
        assert!(!rels.iter().any(|r| r.contains(".next")), "{rels:?}");
        assert!(!rels.iter().any(|r| r.starts_with("out/")), "{rels:?}");
        assert!(!rels.iter().any(|r| r.contains("toolchains")), "{rels:?}");
        let math = manifest
            .entries
            .iter()
            .find(|e| e.relative == "content/index.math")
            .unwrap();
        assert!(math.default_on);
        assert_eq!(math.language, "math");
    }

    #[test]
    fn test_scan_manifest_with_unknown_extension_sniffs_text_and_binary() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("notes.xyzzy"), b"hello\n");
        write(&dir.path().join("blob.xyzzy"), &[0, 1, 2, 3]);
        write(&dir.path().join("bundle.zip"), b"PK\x03\x04");
        let opts = Options {
            roots: vec![dir.path().to_path_buf()],
            gitignore: false,
            ..Options::default()
        };
        let manifest = scan_manifest(&opts).unwrap();
        let notes = manifest
            .entries
            .iter()
            .find(|e| e.relative == "notes.xyzzy")
            .unwrap();
        assert_eq!(notes.kind, Kind::Text);
        assert_eq!(notes.language, "xyzzy");
        assert!(notes.default_on);
        let blob = manifest
            .entries
            .iter()
            .find(|e| e.relative == "blob.xyzzy")
            .unwrap();
        assert_eq!(blob.kind, Kind::Binary);
        assert!(!blob.default_on);
        let zip = manifest
            .entries
            .iter()
            .find(|e| e.relative == "bundle.zip")
            .unwrap();
        assert!(zip.kind.is_archive());
        assert!(!zip.default_on);
    }

    #[test]
    fn test_scan_manifest_with_root_inside_generated_dir_names_sets_default_on_true() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("build/venv/app");
        write(&root.join("src/main.rs"), b"fn main() {}\n");
        write(&root.join("dist/bundle.js"), b"x\n");
        let opts = Options {
            roots: vec![root],
            default_excludes: false,
            ..Options::default()
        };
        let manifest = scan_manifest(&opts).unwrap();
        let main = manifest
            .entries
            .iter()
            .find(|e| e.relative == "src/main.rs")
            .unwrap();
        assert!(main.default_on, "a project under build/venv starts ticked");
        let bundle = manifest
            .entries
            .iter()
            .find(|e| e.relative == "dist/bundle.js")
            .unwrap();
        assert!(
            !bundle.default_on,
            "dist/ inside the root still starts unticked"
        );
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
