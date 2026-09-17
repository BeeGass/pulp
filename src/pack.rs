use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use rayon::prelude::*;

use crate::classify::{Kind, classify, looks_binary};
use crate::config::{Options, TreeMode};
use crate::error::Error;
use crate::extract::{ExtractOpts, expand_archive, extract};
use crate::walk::{self, WalkedFile};

const MAX_ARCHIVE_DEPTH: u8 = 3;
const MAX_ARCHIVE_MEMBERS: usize = 10_000;
const MAX_ARCHIVE_UNCOMPRESSED: u64 = 512 * 1024 * 1024;

/// Outcome of packing one input (or archive member).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileStatus {
    Extracted,
    SkippedBinary,
    TooLarge,
    SkippedArchive,
    Error(String),
}

/// One file (or archive member) in a [`Packed`] dump.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackedFile {
    pub relative: String,
    pub kind: Kind,
    pub size: u64,
    pub text: String,
    pub status: FileStatus,
}

/// Totals for a pack run.
#[derive(Debug, Clone)]
pub struct Stats {
    pub files_extracted: usize,
    pub files_skipped: usize,
    pub bytes_read: u64,
    pub chars_emitted: usize,
    pub tokens_est: usize,
    pub elapsed: Duration,
}

/// Walked, extracted files plus an optional directory map.
#[derive(Debug, Clone)]
pub struct Packed {
    pub files: Vec<PackedFile>,
    pub tree: String,
    pub stats: Stats,
}

struct WorkItem {
    relative: String,
    absolute: Option<PathBuf>,
    bytes: Vec<u8>,
    depth: u8,
}

/// Walk `opts.roots` and extract each file into LLM-readable text.
pub fn pack(opts: &Options) -> Result<Packed, Error> {
    let start = Instant::now();
    let walked = walk::collect(opts)?;
    let extract_opts = ExtractOpts::from_options(opts);
    let bytes_read = AtomicU64::new(0);

    let mut files = run_parallel(opts.jobs, || {
        walked
            .par_iter()
            .flat_map(|wf| process_walked(wf, opts, &extract_opts, &bytes_read))
            .collect::<Vec<PackedFile>>()
    })?;
    files.sort_by(|a, b| a.relative.cmp(&b.relative));

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
            elapsed: start.elapsed(),
        },
    })
}

fn run_parallel<T, F>(jobs: usize, f: F) -> Result<T, Error>
where
    T: Send,
    F: FnOnce() -> T + Send,
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

fn process_walked(
    wf: &WalkedFile,
    opts: &Options,
    extract_opts: &ExtractOpts,
    bytes_read: &AtomicU64,
) -> Vec<PackedFile> {
    if opts.list_only {
        return vec![list_only_file(wf, opts)];
    }
    if wf.size > opts.max_file_size {
        return vec![packed_too_large(
            wf.relative.clone(),
            classify(&wf.absolute, None),
            wf.size,
            opts.max_file_size,
        )];
    }
    let bytes = match read_limited(&wf.absolute, opts.max_file_size) {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(size)) => {
            return vec![packed_too_large(
                wf.relative.clone(),
                classify(&wf.absolute, None),
                size,
                opts.max_file_size,
            )];
        }
        Err(err) => {
            return vec![packed_error(
                wf.relative.clone(),
                classify(&wf.absolute, None),
                wf.size,
                err.to_string(),
            )];
        }
    };
    bytes_read.fetch_add(bytes.len() as u64, Ordering::Relaxed);
    process_item(
        WorkItem {
            relative: wf.relative.clone(),
            absolute: Some(wf.absolute.clone()),
            bytes,
            depth: 0,
        },
        opts,
        extract_opts,
    )
}

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
            &item.relative,
            &item.bytes,
            kind,
            opts,
            extract_opts,
            item.depth,
        );
    }
    vec![pack_one(
        item.relative,
        kind,
        size,
        &item.bytes,
        opts,
        extract_opts,
    )]
}

fn pack_one(
    relative: String,
    kind: Kind,
    size: u64,
    bytes: &[u8],
    opts: &Options,
    extract_opts: &ExtractOpts,
) -> PackedFile {
    if size > opts.max_file_size {
        return packed_too_large(relative, kind, size, opts.max_file_size);
    }
    if should_skip_binary(kind, bytes, opts.skip_binaries) {
        return PackedFile {
            text: format!("[binary file, {size} bytes]"),
            relative,
            kind,
            size,
            status: FileStatus::SkippedBinary,
        };
    }
    if kind.is_archive() {
        return PackedFile {
            text: format!(
                "[archive {relative}, {size} bytes; pass --archives to expand nested archives]"
            ),
            relative,
            kind,
            size,
            status: FileStatus::SkippedArchive,
        };
    }
    match extract(&relative, bytes, kind, extract_opts) {
        Ok(text) => PackedFile {
            relative,
            kind,
            size,
            text,
            status: FileStatus::Extracted,
        },
        Err(err) => packed_error(relative, kind, size, err.to_string()),
    }
}

fn expand_item(
    relative: &str,
    bytes: &[u8],
    kind: Kind,
    opts: &Options,
    extract_opts: &ExtractOpts,
    depth: u8,
) -> Vec<PackedFile> {
    match expand_archive(bytes, kind, extract_opts) {
        Ok(members) => take_archive_members(relative, members, opts, extract_opts, depth),
        Err(err) => vec![packed_error(
            relative.to_string(),
            kind,
            bytes.len() as u64,
            err.to_string(),
        )],
    }
}

fn take_archive_members(
    relative: &str,
    members: Vec<(String, Vec<u8>)>,
    opts: &Options,
    extract_opts: &ExtractOpts,
    depth: u8,
) -> Vec<PackedFile> {
    let include = if opts.include.is_empty() {
        None
    } else {
        walk::build_globset(&opts.include).ok()
    };
    let Ok(exclude) = walk::build_globset(&opts.exclude) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut total = 0u64;
    let mut kept = 0usize;
    for (name, mem_bytes) in members {
        if is_unsafe_entry(&name) {
            continue;
        }
        let child = join_rel(relative, &name);
        if !opts.hidden && walk::is_hidden_rel(&child) {
            continue;
        }
        if kept >= MAX_ARCHIVE_MEMBERS {
            break;
        }
        let n = mem_bytes.len() as u64;
        if total.saturating_add(n) > MAX_ARCHIVE_UNCOMPRESSED {
            break;
        }
        let kind = classify(Path::new(&child), Some(&mem_bytes));
        let emit = !kind.is_archive();
        if emit && !walk::keep_relative(&child, include.as_ref(), &exclude) {
            continue;
        }
        if !emit && glob_exclude_only(&child, &exclude) {
            continue;
        }
        kept += 1;
        total = total.saturating_add(n);
        out.extend(process_item(
            WorkItem {
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
    !walk::keep_relative(relative, None, exclude)
}

fn list_only_file(wf: &WalkedFile, opts: &Options) -> PackedFile {
    let kind = classify(&wf.absolute, None);
    let status = if wf.size > opts.max_file_size {
        FileStatus::TooLarge
    } else if kind.is_archive() {
        FileStatus::SkippedArchive
    } else if opts.skip_binaries && kind == Kind::Binary {
        FileStatus::SkippedBinary
    } else {
        FileStatus::Extracted
    };
    PackedFile {
        relative: wf.relative.clone(),
        kind,
        size: wf.size,
        text: String::new(),
        status,
    }
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

fn tree_label(roots: &[PathBuf]) -> String {
    match roots {
        [] => "pulp".to_string(),
        [root] => root
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| ".".to_string()),
        _ => "pulp".to_string(),
    }
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

fn packed_too_large(relative: String, kind: Kind, size: u64, limit: u64) -> PackedFile {
    PackedFile {
        text: format!("[too large: {size} bytes; limit {limit} bytes]"),
        relative,
        kind,
        size,
        status: FileStatus::TooLarge,
    }
}

fn packed_error(relative: String, kind: Kind, size: u64, message: String) -> PackedFile {
    PackedFile {
        text: format!("[error extracting {relative}: {message}]"),
        relative,
        kind,
        size,
        status: FileStatus::Error(message),
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
        write(&dir.path().join("runs/params.npz"), &[0, 1, 2, 3, 4]);
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
            *r == "runs/params.npz" && *k == Kind::Npz && *s == FileStatus::Extracted
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
    fn test_is_unsafe_entry_with_parent_or_abs_returns_true() {
        assert!(is_unsafe_entry("../etc/passwd"));
        assert!(is_unsafe_entry("/etc/passwd"));
        assert!(is_unsafe_entry("foo/../../bar"));
        assert!(is_unsafe_entry("C:/Windows/system32"));
        assert!(!is_unsafe_entry("foo/bar.txt"));
        assert!(!is_unsafe_entry("dir/file.rs"));
    }
}
