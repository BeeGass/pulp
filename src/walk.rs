use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::SystemTime;

use globset::{Glob, GlobSet, GlobSetBuilder};
use ignore::overrides::OverrideBuilder;
use ignore::{WalkBuilder, WalkState};

use crate::config::Options;
use crate::error::Error;

/// A file discovered under the pack roots.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalkedFile {
    /// Unique within one walk. Independent of display [`Self::relative`].
    pub id: String,
    pub absolute: PathBuf,
    /// `/` separators, no leading `./`.
    pub relative: String,
    pub size: u64,
    pub is_symlink: bool,
    pub modified: Option<SystemTime>,
}

/// Walked files plus whether discovery stopped at a budget.
#[derive(Debug, Clone)]
pub struct WalkOutcome {
    pub files: Vec<WalkedFile>,
    pub truncated: bool,
}

/// Collect files under `opts.roots`, applying gitignore, include, and exclude.
#[must_use = "collecting files has no effect unless the result is used"]
pub fn collect(opts: &Options) -> Result<Vec<WalkedFile>, Error> {
    Ok(collect_detailed(opts)?.files)
}

/// Like [`collect`], but reports whether the discovery budget stopped the walk.
pub fn collect_detailed(opts: &Options) -> Result<WalkOutcome, Error> {
    if opts.roots.is_empty() {
        return Ok(WalkOutcome {
            files: Vec::new(),
            truncated: false,
        });
    }

    let multi = opts.roots.len() > 1;
    let include = if opts.include.is_empty() {
        None
    } else {
        Some(build_globset(&opts.include)?)
    };
    let exclude = build_globset(&opts.exclude)?;
    let skip_paths = normalize_skip_paths(&opts.skip_paths);

    let mut files: Vec<WalkedFile> = Vec::new();
    let mut truncated = false;
    for (idx, root) in opts.roots.iter().enumerate() {
        if !root.exists() {
            return Err(Error::path(root, "does not exist"));
        }
        let remaining_entries = opts.max_entries.saturating_sub(files.len());
        let remaining_bytes = opts
            .max_total_bytes
            .saturating_sub(files.iter().map(|f| f.size).sum::<u64>());
        if remaining_entries == 0 || remaining_bytes == 0 {
            truncated = true;
            break;
        }
        let (chunk, hit) = collect_root(
            root,
            idx,
            opts,
            multi,
            include.as_ref(),
            &exclude,
            WalkLimits {
                max_entries: remaining_entries,
                max_total_bytes: remaining_bytes,
            },
        )?;
        files.extend(chunk);
        if hit {
            truncated = true;
            break;
        }
    }
    match &opts.selection {
        crate::config::Selection::AllEligible => {}
        crate::config::Selection::Only(ids) if ids.is_empty() => files.clear(),
        crate::config::Selection::Only(ids) => {
            let want: HashSet<&str> = ids.iter().map(String::as_str).collect();
            files.retain(|file| {
                want.contains(file.id.as_str()) || want.contains(file.relative.as_str())
            });
        }
    }
    if !skip_paths.is_empty() {
        files.retain(|file| !is_skipped_path(&file.absolute, &skip_paths));
    }
    files.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(WalkOutcome { files, truncated })
}

/// Canonicalize skip destinations once so each candidate is compared cheaply.
#[must_use]
pub fn normalize_skip_paths(paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for path in paths {
        let key = path.canonicalize().unwrap_or_else(|_| path.clone());
        if seen.insert(key.clone()) {
            out.push(key);
        }
    }
    out
}

#[derive(Clone, Copy)]
struct WalkLimits {
    max_entries: usize,
    max_total_bytes: u64,
}

fn collect_root(
    root: &Path,
    root_idx: usize,
    opts: &Options,
    multi: bool,
    include: Option<&GlobSet>,
    exclude: &GlobSet,
    limits: WalkLimits,
) -> Result<(Vec<WalkedFile>, bool), Error> {
    let meta = fs::symlink_metadata(root).map_err(|err| Error::path(root, err.to_string()))?;
    let is_symlink = meta.file_type().is_symlink();
    let followed = if is_symlink {
        fs::metadata(root).map_err(|err| Error::path(root, err.to_string()))?
    } else {
        meta.clone()
    };

    if followed.is_file() {
        if is_symlink && !opts.follow_links {
            return Ok((Vec::new(), false));
        }
        let file = walked_file(root, root, root_idx, multi, followed.len(), is_symlink);
        let truncated = 1 > limits.max_entries || file.size > limits.max_total_bytes;
        if truncated {
            return Ok((Vec::new(), true));
        }
        return Ok((vec![file], false));
    }

    if !followed.is_dir() {
        return Err(Error::path(root, "not a file or directory"));
    }

    walk_dir(root, root_idx, opts, multi, include, exclude, limits)
}

fn walk_dir(
    root: &Path,
    root_idx: usize,
    opts: &Options,
    multi: bool,
    include: Option<&GlobSet>,
    exclude: &GlobSet,
    limits: WalkLimits,
) -> Result<(Vec<WalkedFile>, bool), Error> {
    let mut builder = WalkBuilder::new(root);
    builder
        .hidden(!opts.hidden)
        .git_ignore(opts.gitignore)
        .git_global(opts.gitignore)
        .git_exclude(opts.gitignore)
        .ignore(opts.gitignore)
        .follow_links(opts.follow_links)
        .threads(opts.jobs)
        .filter_entry(|entry| entry.file_name() != OsStr::new(".git"));

    if !opts.exclude.is_empty() {
        let mut overrides = OverrideBuilder::new(root);
        for pat in &opts.exclude {
            if pat.is_empty() {
                continue;
            }
            let glob = if pat.starts_with('!') {
                pat.clone()
            } else {
                format!("!{pat}")
            };
            overrides
                .add(&glob)
                .map_err(|err| Error::msg(format!("invalid exclude glob {pat}: {err}")))?;
        }
        builder.overrides(
            overrides
                .build()
                .map_err(|err| Error::msg(err.to_string()))?,
        );
    }

    let include = include.cloned().map(Arc::new);
    let exclude = Arc::new(exclude.clone());
    let follow_links = opts.follow_links;
    let follow_archives = opts.follow_archives;
    let root_buf = root.to_path_buf();
    let count = Arc::new(AtomicUsize::new(0));
    let bytes = Arc::new(AtomicU64::new(0));
    let truncated = Arc::new(AtomicBool::new(false));

    let (tx, rx) = crossbeam_channel::unbounded::<WalkedFile>();
    builder.build_parallel().run(|| {
        let tx = tx.clone();
        let include = include.clone();
        let exclude = Arc::clone(&exclude);
        let root_buf = root_buf.clone();
        let count = Arc::clone(&count);
        let bytes = Arc::clone(&bytes);
        let truncated = Arc::clone(&truncated);
        Box::new(move |result| {
            let Ok(entry) = result else {
                return WalkState::Continue;
            };
            if entry.file_type().is_none_or(|ft| !ft.is_file()) {
                return WalkState::Continue;
            }
            let is_symlink = entry.path_is_symlink();
            if is_symlink && !follow_links {
                return WalkState::Continue;
            }
            if is_in_git_dir(entry.path()) {
                return WalkState::Continue;
            }
            let meta = match entry.metadata() {
                Ok(meta) => meta,
                Err(_) => return WalkState::Continue,
            };
            let size = meta.len();
            let modified = meta.modified().ok();
            let relative = relative_for(&root_buf, entry.path(), multi);
            if !keep_for_walk(
                &relative,
                include.as_deref(),
                exclude.as_ref(),
                follow_archives,
            ) {
                return WalkState::Continue;
            }
            if count.load(Ordering::Relaxed) >= limits.max_entries {
                truncated.store(true, Ordering::Relaxed);
                return WalkState::Quit;
            }
            loop {
                let cur = bytes.load(Ordering::Relaxed);
                if cur.saturating_add(size) > limits.max_total_bytes {
                    truncated.store(true, Ordering::Relaxed);
                    return WalkState::Quit;
                }
                if bytes
                    .compare_exchange_weak(
                        cur,
                        cur.saturating_add(size),
                        Ordering::Relaxed,
                        Ordering::Relaxed,
                    )
                    .is_ok()
                {
                    break;
                }
            }
            let n = count.fetch_add(1, Ordering::Relaxed);
            if n >= limits.max_entries {
                truncated.store(true, Ordering::Relaxed);
                return WalkState::Quit;
            }
            let file = WalkedFile {
                id: file_id(root_idx, multi, &relative),
                absolute: make_absolute(entry.path()),
                relative,
                size,
                is_symlink,
                modified,
            };
            if tx.send(file).is_err() {
                return WalkState::Quit;
            }
            WalkState::Continue
        })
    });
    drop(tx);

    let mut files: Vec<WalkedFile> = rx.iter().collect();
    files.retain(|file| {
        keep_for_walk(
            &file.relative,
            include.as_deref(),
            exclude.as_ref(),
            follow_archives,
        )
    });
    Ok((files, truncated.load(Ordering::Relaxed)))
}

fn walked_file(
    root: &Path,
    path: &Path,
    root_idx: usize,
    multi: bool,
    size: u64,
    is_symlink: bool,
) -> WalkedFile {
    let relative = relative_for(root, path, multi);
    let modified = std::fs::metadata(path).ok().and_then(|m| m.modified().ok());
    WalkedFile {
        id: file_id(root_idx, multi, &relative),
        absolute: make_absolute(path),
        relative,
        size,
        is_symlink,
        modified,
    }
}

fn file_id(root_idx: usize, multi: bool, relative: &str) -> String {
    if multi {
        format!("{root_idx}:{relative}")
    } else {
        relative.to_string()
    }
}

/// Whether a filesystem path may enter the pipeline (emit or traverse).
pub(crate) fn keep_for_walk(
    relative: &str,
    include: Option<&GlobSet>,
    exclude: &GlobSet,
    follow_archives: bool,
) -> bool {
    if glob_matches(exclude, relative) {
        return false;
    }
    match include {
        None => true,
        Some(set) => {
            glob_matches(set, relative) || (follow_archives && looks_like_archive(relative))
        }
    }
}

pub(crate) fn keep_relative(relative: &str, include: Option<&GlobSet>, exclude: &GlobSet) -> bool {
    if glob_matches(exclude, relative) {
        return false;
    }
    match include {
        None => true,
        Some(set) => glob_matches(set, relative),
    }
}

fn looks_like_archive(relative: &str) -> bool {
    let n = relative.to_ascii_lowercase();
    n.ends_with(".zip") || n.ends_with(".tar") || n.ends_with(".tgz") || n.ends_with(".tar.gz")
}

fn glob_matches(set: &GlobSet, relative: &str) -> bool {
    if set.is_match(relative) {
        return true;
    }
    Path::new(relative)
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| set.is_match(name))
}

pub(crate) fn build_globset(patterns: &[String]) -> Result<GlobSet, Error> {
    let mut builder = GlobSetBuilder::new();
    for pat in patterns {
        if pat.is_empty() {
            continue;
        }
        add_glob(&mut builder, pat)?;
    }
    builder.build().map_err(|err| Error::msg(err.to_string()))
}

fn add_glob(builder: &mut GlobSetBuilder, pat: &str) -> Result<(), Error> {
    let glob = Glob::new(pat).map_err(|err| Error::msg(format!("invalid glob {pat}: {err}")))?;
    builder.add(glob);
    let trimmed = pat.trim_start_matches('/');
    if !pat.starts_with("**/") && !trimmed.is_empty() {
        let nested = format!("**/{trimmed}");
        if nested != pat {
            if let Ok(glob) = Glob::new(&nested) {
                builder.add(glob);
            }
        }
    }
    Ok(())
}

fn relative_for(root: &Path, path: &Path, multi: bool) -> String {
    let stripped = path.strip_prefix(root).unwrap_or(path);
    let rel = normalize_rel(&stripped.to_string_lossy());
    if multi {
        let prefix = root_label(root);
        if rel.is_empty() {
            prefix
        } else {
            format!("{prefix}/{rel}")
        }
    } else if rel.is_empty() {
        root.file_name()
            .map(|name| name.to_string_lossy().replace('\\', "/"))
            .filter(|name| !name.is_empty())
            .unwrap_or(rel)
    } else {
        rel
    }
}

fn root_label(root: &Path) -> String {
    root.file_name()
        .map(|name| name.to_string_lossy().replace('\\', "/"))
        .filter(|name| !name.is_empty() && name != ".")
        .unwrap_or_else(|| "root".to_string())
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

fn make_absolute(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    }
}

pub(crate) fn is_hidden_rel(relative: &str) -> bool {
    relative
        .split('/')
        .any(|part| part.starts_with('.') && part != "." && part != "..")
}

fn is_skipped_path(path: &Path, skip: &[PathBuf]) -> bool {
    skip.iter().any(|other| paths_equal(path, other))
}

fn paths_equal(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

fn is_in_git_dir(path: &Path) -> bool {
    path.components().any(|c| c.as_os_str() == ".git")
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

    fn opts_for(root: PathBuf) -> Options {
        Options {
            roots: vec![root],
            ..Options::default()
        }
    }

    #[test]
    fn test_collect_with_node_modules_skips_nested_files() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("src/lib.rs"), b"pub fn x() {}\n");
        write(
            &dir.path().join("node_modules/leftpad/index.js"),
            b"module.exports=1;\n",
        );

        let files = collect(&opts_for(dir.path().to_path_buf())).unwrap();
        let rels: Vec<&str> = files.iter().map(|f| f.relative.as_str()).collect();
        assert!(rels.contains(&"src/lib.rs"));
        assert!(
            !rels
                .iter()
                .any(|r| r.split('/').any(|p| p == "node_modules"))
        );
    }

    #[test]
    fn test_collect_with_rs_lean_npz_returns_those_files() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("src/lib.rs"), b"pub fn x() {}\n");
        write(&dir.path().join("Math/Basic.lean"), b"def x := 1\n");
        write(&dir.path().join("runs/params.npz"), b"PK\x03\x04npy");
        write(
            &dir.path().join("cache/batch.npy"),
            &[0x93, b'N', b'U', b'M', b'P', b'Y'],
        );
        write(&dir.path().join("target/debug/foo.rs"), b"fn main() {}\n");
        write(
            &dir.path().join("node_modules/pkg/index.js"),
            b"module.exports=1;\n",
        );

        let files = collect(&opts_for(dir.path().to_path_buf())).unwrap();
        let rels: Vec<&str> = files.iter().map(|f| f.relative.as_str()).collect();
        assert!(rels.contains(&"src/lib.rs"));
        assert!(rels.contains(&"Math/Basic.lean"));
        assert!(rels.contains(&"runs/params.npz"));
        assert!(rels.contains(&"cache/batch.npy"));
        assert!(!rels.iter().any(|r| r.starts_with("target/")));
        assert!(
            !rels
                .iter()
                .any(|r| r.split('/').any(|p| p == "node_modules"))
        );
    }

    #[test]
    fn test_collect_with_selected_paths_returns_only_those_files() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("keep.rs"), b"fn keep() {}\n");
        write(&dir.path().join("drop.rs"), b"fn drop() {}\n");
        write(&dir.path().join("Basic.lean"), b"def n := 0\n");
        let opts = Options {
            roots: vec![dir.path().to_path_buf()],
            selection: crate::config::Selection::Only(vec!["keep.rs".into(), "Basic.lean".into()]),
            ..Options::default()
        };
        let files = collect(&opts).unwrap();
        let rels: Vec<&str> = files.iter().map(|f| f.relative.as_str()).collect();
        assert_eq!(rels, vec!["Basic.lean", "keep.rs"]);
        assert_eq!(files[0].id, "Basic.lean");
    }

    #[test]
    fn test_collect_with_two_same_basename_roots_returns_distinct_ids() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("one/repo");
        let b = dir.path().join("two/repo");
        write(&a.join("src/lib.rs"), b"fn a() {}\n");
        write(&b.join("src/lib.rs"), b"fn b() {}\n");
        let opts = Options {
            roots: vec![a, b],
            ..Options::default()
        };
        let files = collect(&opts).unwrap();
        let ids: Vec<&str> = files.iter().map(|f| f.id.as_str()).collect();
        assert_eq!(ids.len(), 2);
        assert_ne!(ids[0], ids[1]);
        assert!(ids.iter().all(|id| id.contains(':')), "{ids:?}");
    }

    #[test]
    fn test_collect_with_include_rs_keeps_zip_when_archives_follow() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("src/lib.rs"), b"fn x() {}\n");
        write(&dir.path().join("bundle.zip"), b"PK\x03\x04");
        write(&dir.path().join("notes.txt"), b"hi\n");
        let opts = Options {
            roots: vec![dir.path().to_path_buf()],
            include: vec!["*.rs".into()],
            follow_archives: true,
            exclude: Vec::new(),
            ..Options::default()
        };
        let files = collect(&opts).unwrap();
        let rels: Vec<&str> = files.iter().map(|f| f.relative.as_str()).collect();
        assert!(rels.contains(&"src/lib.rs"), "{rels:?}");
        assert!(rels.contains(&"bundle.zip"), "{rels:?}");
        assert!(!rels.contains(&"notes.txt"), "{rels:?}");
    }

    #[test]
    fn test_collect_detailed_with_max_entries_stops_walk() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("a.rs"), b"fn a() {}\n");
        write(&dir.path().join("b.rs"), b"fn b() {}\n");
        write(&dir.path().join("c.rs"), b"fn c() {}\n");
        let opts = Options {
            roots: vec![dir.path().to_path_buf()],
            max_entries: 1,
            exclude: Vec::new(),
            ..Options::default()
        };
        let outcome = collect_detailed(&opts).unwrap();
        assert!(outcome.truncated);
        assert_eq!(outcome.files.len(), 1);
    }

    #[test]
    fn test_collect_with_empty_only_returns_no_files() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("keep.rs"), b"fn keep() {}\n");
        let opts = Options {
            roots: vec![dir.path().to_path_buf()],
            selection: crate::config::Selection::Only(Vec::new()),
            ..Options::default()
        };
        let files = collect(&opts).unwrap();
        assert!(files.is_empty());
    }

    #[test]
    fn test_collect_with_missing_root_returns_path_error() {
        let opts = Options {
            roots: vec![PathBuf::from("/nonexistent/pulp-walk-missing-root")],
            ..Options::default()
        };
        let err = collect(&opts).unwrap_err();
        assert!(matches!(err, Error::Path { .. }));
    }

    #[test]
    fn test_collect_with_file_root_returns_file_name() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("solo.lean");
        write(&path, b"def y := 2\n");
        let files = collect(&opts_for(path)).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].relative, "solo.lean");
    }

    #[test]
    fn test_collect_with_hidden_still_skips_git_dir() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("src/a.rs"), b"fn a() {}\n");
        write(&dir.path().join(".git/objects/ab"), b"blob");
        let mut opts = opts_for(dir.path().to_path_buf());
        opts.hidden = true;
        opts.exclude = Vec::new();
        let files = collect(&opts).unwrap();
        let rels: Vec<&str> = files.iter().map(|f| f.relative.as_str()).collect();
        assert!(rels.contains(&"src/a.rs"));
        assert!(!rels.iter().any(|r| r.split('/').any(|p| p == ".git")));
    }

    #[test]
    fn test_collect_with_gitignore_false_includes_dot_ignore_matches() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join(".ignore"), b"secret.txt\n");
        write(&dir.path().join("secret.txt"), b"nope\n");
        write(&dir.path().join("ok.rs"), b"fn ok() {}\n");
        let mut opts = opts_for(dir.path().to_path_buf());
        opts.gitignore = false;
        opts.exclude = Vec::new();
        let files = collect(&opts).unwrap();
        let rels: Vec<&str> = files.iter().map(|f| f.relative.as_str()).collect();
        assert!(rels.contains(&"ok.rs"));
        assert!(
            rels.contains(&"secret.txt"),
            "--no-gitignore must also disable .ignore, got {rels:?}"
        );
    }

    #[test]
    fn test_collect_with_multiple_roots_prefixes_file_name() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("proj_a");
        let b = dir.path().join("proj_b");
        write(&a.join("src/lib.rs"), b"fn a() {}\n");
        write(&b.join("src/lib.rs"), b"fn b() {}\n");
        let opts = Options {
            roots: vec![a, b],
            ..Options::default()
        };
        let files = collect(&opts).unwrap();
        let rels: Vec<&str> = files.iter().map(|f| f.relative.as_str()).collect();
        assert!(rels.contains(&"proj_a/src/lib.rs"));
        assert!(rels.contains(&"proj_b/src/lib.rs"));
    }
}
