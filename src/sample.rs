//! The built-in sample project behind the mill's "Try a sample" key.
//!
//! One bundle, `web/sample.json`, feeds both mills: the browser mill fetches it,
//! and the local mill writes it to a temp folder so the normal scan path runs.

use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;

use serde::Deserialize;

const BUNDLE: &str = include_str!("../web/sample.json");

#[derive(Deserialize)]
struct Bundle {
    name: String,
    files: Vec<BundleFile>,
}

#[derive(Deserialize)]
struct BundleFile {
    path: String,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    base64: Option<String>,
}

/// This process's private copy of the sample. Created on first use and
/// removed when the mill stops.
#[derive(Default)]
pub struct Scratch(Mutex<Option<Private>>);

/// A folder this process created, and the identity it had then.
struct Private {
    path: PathBuf,
    id: DirId,
}

#[cfg(unix)]
type DirId = (u64, u64);
#[cfg(not(unix))]
type DirId = ();

fn dir_id(meta: &std::fs::Metadata) -> DirId {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        (meta.dev(), meta.ino())
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
    }
}

impl Private {
    /// Whether the path still names the real folder this process made. A temp
    /// cleaner may delete it mid-session, and whatever appears under the same
    /// name afterwards (a symlink, say) is someone else's.
    fn is_intact(&self) -> bool {
        std::fs::symlink_metadata(&self.path)
            .is_ok_and(|meta| meta.is_dir() && dir_id(&meta) == self.id)
    }
}

impl Scratch {
    /// Write the sample into the private folder and return the project root.
    pub fn materialize(&self) -> Result<PathBuf, String> {
        let mut slot = self
            .0
            .lock()
            .map_err(|_| "sample folder lock poisoned".to_string())?;
        if !slot.as_ref().is_some_and(Private::is_intact) {
            let fresh = create_private_dir(&std::env::temp_dir())
                .map_err(|err| format!("create a sample folder: {err}"))?;
            *slot = Some(fresh);
        }
        match slot.as_ref() {
            Some(private) => materialize(&private.path),
            None => Err("sample folder missing".into()),
        }
    }

    /// Delete the private folder, if one was made and it is still ours.
    pub fn remove(&self) {
        if let Ok(mut slot) = self.0.lock() {
            if let Some(private) = slot.take() {
                if private.is_intact() {
                    let _ = std::fs::remove_dir_all(&private.path);
                }
            }
        }
    }
}

/// Make a new owner-only folder with a random name under `parent`.
///
/// `create` fails when the name exists, so a path planted in a shared temp
/// folder (a symlink, say) is never followed. Only this user can write in the
/// new folder, so nothing can be planted inside it either.
fn create_private_dir(parent: &Path) -> std::io::Result<Private> {
    let mut buf = [0u8; 8];
    getrandom::fill(&mut buf).map_err(|err| std::io::Error::other(err.to_string()))?;
    let name = buf
        .iter()
        .fold(String::from("pulp-sample-"), |mut name, b| {
            name.push_str(&format!("{b:02x}"));
            name
        });
    let dir = parent.join(name);
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(&dir)?;
    let id = dir_id(&std::fs::symlink_metadata(&dir)?);
    Ok(Private { path: dir, id })
}

/// Write the sample project under `parent` and return its root folder.
///
/// Files that already hold the right bytes are left alone, so a second "Try a
/// sample" does not make an open scan of the same copy look changed.
pub fn materialize(parent: &Path) -> Result<PathBuf, String> {
    let bundle: Bundle =
        serde_json::from_str(BUNDLE).map_err(|err| format!("sample bundle: {err}"))?;
    let root = parent.join(safe_relative(&bundle.name)?);
    for file in &bundle.files {
        let relative = safe_relative(&file.path)?;
        let bytes = file_bytes(file)?;
        let dest = root.join(relative);
        if std::fs::read(&dest).is_ok_and(|current| current == bytes) {
            continue;
        }
        if let Some(dir) = dest.parent() {
            std::fs::create_dir_all(dir)
                .map_err(|err| format!("create {}: {err}", dir.display()))?;
        }
        std::fs::write(&dest, bytes).map_err(|err| format!("write {}: {err}", dest.display()))?;
    }
    Ok(root)
}

fn file_bytes(file: &BundleFile) -> Result<Vec<u8>, String> {
    match (&file.text, &file.base64) {
        (Some(text), None) => Ok(text.as_bytes().to_vec()),
        (None, Some(encoded)) => {
            decode_base64(encoded).map_err(|err| format!("{}: {err}", file.path))
        }
        _ => Err(format!(
            "{}: a sample file needs exactly one of text or base64",
            file.path
        )),
    }
}

fn safe_relative(path: &str) -> Result<&Path, String> {
    let relative = Path::new(path);
    let plain = relative
        .components()
        .all(|part| matches!(part, Component::Normal(_)));
    if path.is_empty() || !plain {
        return Err(format!("sample path {path:?} must stay inside the project"));
    }
    Ok(relative)
}

fn decode_base64(encoded: &str) -> Result<Vec<u8>, String> {
    fn value(byte: u8) -> Option<u32> {
        match byte {
            b'A'..=b'Z' => Some(u32::from(byte - b'A')),
            b'a'..=b'z' => Some(u32::from(byte - b'a') + 26),
            b'0'..=b'9' => Some(u32::from(byte - b'0') + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let clean: Vec<u8> = encoded
        .bytes()
        .filter(|b| !b.is_ascii_whitespace())
        .collect();
    if clean.len() % 4 != 0 {
        return Err("base64 length is not a multiple of 4".into());
    }
    let mut out = Vec::with_capacity(clean.len() / 4 * 3);
    for quad in clean.chunks(4) {
        let pad = quad.iter().rev().take_while(|&&b| b == b'=').count();
        if pad > 2 || quad[..4 - pad].contains(&b'=') {
            return Err("misplaced base64 padding".into());
        }
        let mut acc = 0u32;
        for &byte in &quad[..4 - pad] {
            let bits = value(byte).ok_or_else(|| format!("invalid base64 byte {byte:#04x}"))?;
            acc = (acc << 6) | bits;
        }
        acc <<= 6 * pad as u32;
        let bytes = acc.to_be_bytes();
        out.extend_from_slice(&bytes[1..4 - pad]);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_scratch_reuses_one_private_folder_then_removes_it() {
        let scratch = Scratch::default();
        let first = scratch.materialize().unwrap();
        let second = scratch.materialize().unwrap();
        assert_eq!(first, second);
        let parent = first.parent().unwrap().to_path_buf();
        assert!(
            parent
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("pulp-sample-")
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&parent).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o700);
        }
        scratch.remove();
        assert!(!parent.exists());
    }

    #[test]
    fn test_create_private_dir_uses_fresh_random_names() {
        let dir = tempfile::tempdir().unwrap();
        let a = create_private_dir(dir.path()).unwrap();
        let b = create_private_dir(dir.path()).unwrap();
        assert_ne!(a.path, b.path);
        assert!(a.path.is_dir() && b.path.is_dir());
        assert!(a.is_intact() && b.is_intact());
    }

    #[cfg(unix)]
    #[test]
    fn test_scratch_with_replaced_folder_never_writes_through_a_symlink() {
        let scratch = Scratch::default();
        let first = scratch.materialize().unwrap();
        let parent = first.parent().unwrap().to_path_buf();
        // A temp cleaner removes the folder and someone plants a link in its place.
        let elsewhere = tempfile::tempdir().unwrap();
        std::fs::remove_dir_all(&parent).unwrap();
        std::os::unix::fs::symlink(elsewhere.path(), &parent).unwrap();
        let second = scratch.materialize().unwrap();
        assert_ne!(second.parent().unwrap(), parent.as_path());
        assert!(second.join("README.md").is_file());
        assert!(
            std::fs::read_dir(elsewhere.path())
                .unwrap()
                .next()
                .is_none()
        );
        // Shutdown removes the new private folder and leaves the planted link alone.
        let fresh = second.parent().unwrap().to_path_buf();
        scratch.remove();
        assert!(!fresh.exists());
        assert!(
            std::fs::symlink_metadata(&parent)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        std::fs::remove_file(&parent).unwrap();
    }

    #[test]
    fn test_materialize_again_leaves_unchanged_files_alone() {
        let dir = tempfile::tempdir().unwrap();
        let root = materialize(dir.path()).unwrap();
        let readme = root.join("README.md");
        let old = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
        std::fs::File::options()
            .write(true)
            .open(&readme)
            .unwrap()
            .set_modified(old)
            .unwrap();
        materialize(dir.path()).unwrap();
        assert_eq!(std::fs::metadata(&readme).unwrap().modified().unwrap(), old);
    }

    #[test]
    fn test_decode_base64_with_known_vectors_returns_bytes() {
        assert_eq!(decode_base64("").unwrap(), b"");
        assert_eq!(decode_base64("Zg==").unwrap(), b"f");
        assert_eq!(decode_base64("Zm8=").unwrap(), b"fo");
        assert_eq!(decode_base64("Zm9vYmFy").unwrap(), b"foobar");
        assert_eq!(decode_base64("aGVs\nbG8=").unwrap(), b"hello");
    }

    #[test]
    fn test_decode_base64_with_bad_input_returns_error() {
        assert!(decode_base64("Zm9").is_err());
        assert!(decode_base64("Zm9v!mFy").is_err());
        assert!(decode_base64("Z===").is_err());
        assert!(decode_base64("Z=9v").is_err());
    }

    #[test]
    fn test_safe_relative_with_escaping_paths_returns_error() {
        assert!(safe_relative("src/lib.rs").is_ok());
        assert!(safe_relative("../outside").is_err());
        assert!(safe_relative("/etc/passwd").is_err());
        assert!(safe_relative("a/./b").is_ok());
        assert!(safe_relative("").is_err());
    }

    #[test]
    fn test_materialize_writes_every_bundle_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = materialize(dir.path()).unwrap();
        assert!(root.ends_with("tides"));
        let bundle: Bundle = serde_json::from_str(BUNDLE).unwrap();
        for file in &bundle.files {
            let written = std::fs::read(root.join(&file.path)).unwrap();
            assert_eq!(written, file_bytes(file).unwrap(), "{}", file.path);
        }
        let pdf = std::fs::read(root.join("docs/field-notes.pdf")).unwrap();
        assert!(pdf.starts_with(b"%PDF-"));
        let docx = std::fs::read(root.join("docs/briefing.docx")).unwrap();
        assert!(docx.starts_with(b"PK"));
    }

    /// The site's product shot renders this scan and pack of the sample.
    fn demo_snapshot() -> String {
        use crate::config::{Options, OutputFormat, Selection, TreeMode, default_exclude_globs};
        let dir = tempfile::tempdir().unwrap();
        let root = materialize(dir.path()).unwrap();
        let scan = Options {
            roots: vec![root.clone()],
            list_only: true,
            exclude: default_exclude_globs(),
            ..Options::default()
        };
        let manifest = crate::manifest::scan_manifest(&scan).unwrap();
        let selected: Vec<String> = manifest
            .entries
            .iter()
            .filter(|e| e.default_on && !e.oversized)
            .map(|e| e.id.clone())
            .collect();
        let opts = Options {
            roots: vec![root],
            format: OutputFormat::Xml,
            tree: TreeMode::Selected,
            selection: Selection::Only(selected),
            exclude: default_exclude_globs(),
            ..Options::default()
        };
        let packed = crate::pack::pack(&opts).unwrap();
        let mut out = Vec::new();
        crate::render::write_all(&mut out, &packed, &opts).unwrap();
        let dump = String::from_utf8(out).unwrap();
        let dump_bytes = dump.len();
        let files: Vec<serde_json::Value> = manifest
            .entries
            .iter()
            .map(|e| {
                serde_json::json!({
                    "id": e.id,
                    "relative": e.relative,
                    "size": e.size,
                    "kind": e.kind.as_str(),
                    "language": e.language,
                    "default_on": e.default_on,
                    "oversized": e.oversized,
                })
            })
            .collect();
        let outcomes: Vec<serde_json::Value> = packed
            .files
            .iter()
            .map(|f| {
                serde_json::json!({
                    "id": f.id,
                    "relative": f.relative,
                    "status": f.status.as_str(),
                    "kind": f.kind.as_str(),
                    "language": crate::language_name(Path::new(&f.relative), f.kind),
                    "size": f.size,
                    "message": f.status.message(f.size),
                })
            })
            .collect();
        let doc = serde_json::json!({
            "path": "~/Projects/tides",
            "files": files,
            "pack": {
                "dump": dump,
                "filename": "pulp.xml",
                "dump_bytes": dump_bytes,
                "files_extracted": packed.stats.files_extracted,
                "files_skipped": packed.stats.files_skipped,
                "tokens_est": packed.stats.tokens_est,
                "outcomes": outcomes,
            },
        });
        serde_json::to_string_pretty(&doc).unwrap() + "\n"
    }

    #[test]
    fn test_sample_demo_snapshot_is_current() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("web/sample-demo.json");
        let fresh = demo_snapshot();
        if std::env::var_os("PULP_BLESS").is_some() {
            std::fs::write(&path, &fresh).unwrap();
            return;
        }
        let committed = std::fs::read_to_string(&path).unwrap_or_default();
        assert!(
            committed == fresh,
            "web/sample-demo.json is stale; run `PULP_BLESS=1 cargo test --lib sample` then `cargo xtask site`"
        );
    }

    #[test]
    fn test_materialize_twice_overwrites_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let first = materialize(dir.path()).unwrap();
        std::fs::write(first.join("README.md"), b"edited").unwrap();
        let second = materialize(dir.path()).unwrap();
        assert_eq!(first, second);
        let readme = std::fs::read_to_string(second.join("README.md")).unwrap();
        assert!(readme.starts_with("# tides"));
    }
}
