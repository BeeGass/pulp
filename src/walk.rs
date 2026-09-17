use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use globset::{Glob, GlobSet, GlobSetBuilder};
use ignore::overrides::OverrideBuilder;
use ignore::{WalkBuilder, WalkState};

use crate::config::Options;
use crate::error::Error;

/// A file discovered under the pack roots.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalkedFile {
    pub absolute: PathBuf,
    /// `/` separators, no leading `./`.
    pub relative: String,
    pub size: u64,
    pub is_symlink: bool,
}

/// Collect files under `opts.roots`, applying gitignore, include, and exclude.
#[must_use = "collecting files has no effect unless the result is used"]
pub fn collect(opts: &Options) -> Result<Vec<WalkedFile>, Error> {
    if opts.roots.is_empty() {
        return Ok(Vec::new());
    }

    let multi = opts.roots.len() > 1;
    let include = if opts.include.is_empty() {
        None
    } else {
        Some(build_globset(&opts.include)?)
    };
    let exclude = build_globset(&opts.exclude)?;

    let mut files = Vec::new();
    for root in &opts.roots {
        if !root.exists() {
            return Err(Error::path(root, "does not exist"));
        }
        files.extend(collect_root(root, opts, multi, include.as_ref(), &exclude)?);
    }
    files.sort_by(|a, b| a.relative.cmp(&b.relative));
    Ok(files)
}

fn collect_root(
    root: &Path,
    opts: &Options,
    multi: bool,
    include: Option<&GlobSet>,
    exclude: &GlobSet,
) -> Result<Vec<WalkedFile>, Error> {
    let meta = fs::symlink_metadata(root).map_err(|err| Error::path(root, err.to_string()))?;
    let is_symlink = meta.file_type().is_symlink();
    let followed = if is_symlink {
        fs::metadata(root).map_err(|err| Error::path(root, err.to_string()))?
    } else {
        meta.clone()
    };

    if followed.is_file() {
        if is_symlink && !opts.follow_links {
            return Ok(Vec::new());
        }
        return Ok(vec![walked_file(
            root,
            root,
            multi,
            followed.len(),
            is_symlink,
        )]);
    }

    if !followed.is_dir() {
        return Err(Error::path(root, "not a file or directory"));
    }

    walk_dir(root, opts, multi, include, exclude)
}

fn walk_dir(
    root: &Path,
    opts: &Options,
    multi: bool,
    include: Option<&GlobSet>,
    exclude: &GlobSet,
) -> Result<Vec<WalkedFile>, Error> {
    let mut builder = WalkBuilder::new(root);
    builder
        .hidden(!opts.hidden)
        .git_ignore(opts.gitignore)
        .git_global(opts.gitignore)
        .git_exclude(opts.gitignore)
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
    let root_buf = root.to_path_buf();

    let (tx, rx) = crossbeam_channel::unbounded::<WalkedFile>();
    builder.build_parallel().run(|| {
        let tx = tx.clone();
        let include = include.clone();
        let exclude = Arc::clone(&exclude);
        let root_buf = root_buf.clone();
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
            let size = match entry.metadata() {
                Ok(meta) => meta.len(),
                Err(_) => return WalkState::Continue,
            };
            let relative = relative_for(&root_buf, entry.path(), multi);
            if !keep_relative(&relative, include.as_deref(), exclude.as_ref()) {
                return WalkState::Continue;
            }
            let file = WalkedFile {
                absolute: make_absolute(entry.path()),
                relative,
                size,
                is_symlink,
            };
            if tx.send(file).is_err() {
                return WalkState::Quit;
            }
            WalkState::Continue
        })
    });
    drop(tx);

    let mut files: Vec<WalkedFile> = rx.iter().collect();
    files.retain(|file| keep_relative(&file.relative, include.as_deref(), exclude.as_ref()));
    Ok(files)
}

fn walked_file(root: &Path, path: &Path, multi: bool, size: u64, is_symlink: bool) -> WalkedFile {
    WalkedFile {
        absolute: make_absolute(path),
        relative: relative_for(root, path, multi),
        size,
        is_symlink,
    }
}

fn keep_relative(relative: &str, include: Option<&GlobSet>, exclude: &GlobSet) -> bool {
    if glob_matches(exclude, relative) {
        return false;
    }
    match include {
        None => true,
        Some(set) => glob_matches(set, relative),
    }
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

fn build_globset(patterns: &[String]) -> Result<GlobSet, Error> {
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
