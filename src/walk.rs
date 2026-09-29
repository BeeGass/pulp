use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use globset::GlobSet;
use ignore::WalkBuilder;
use ignore::overrides::OverrideBuilder;

use crate::config::{Budget, Options, Selection};
use crate::error::Error;
use crate::filter::{build_globset, glob_matches};

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

/// Like [`collect`], but reports whether a budget stopped the walk.
///
/// Roots are walked one after another, each depth first with every
/// directory's entries in name order: the path order of
/// [`crate::config::cmp_path_order`]. The entry and byte budgets are spent
/// as files are found, and the walk stops at the first file that does not
/// fit, so a budget bounds the walk's time and memory, and the same tree
/// and options keep the same files whatever [`Options::jobs`] is.
pub fn collect_detailed(opts: &Options) -> Result<WalkOutcome, Error> {
    // Every root is checked before any is walked, so a bad root fails the
    // walk even when a budget would stop it first.
    let kinds = opts
        .roots
        .iter()
        .map(|root| RootKind::of(root))
        .collect::<Result<Vec<_>, _>>()?;
    let mut found = Found::new(opts);
    if kinds.is_empty() || opts.selection.is_empty_only() {
        return Ok(found.finish());
    }
    let include = if opts.include.is_empty() {
        None
    } else {
        Some(build_globset(&opts.include)?)
    };
    let exclude = build_globset(&opts.exclude)?;
    let labels: Vec<String> = opts.roots.iter().map(|root| root_label(root)).collect();
    let multi = kinds.len() > 1;
    for (idx, (root, kind)) in opts.roots.iter().zip(kinds).enumerate() {
        let scope = RootScope {
            root,
            absolute: make_absolute(root),
            idx,
            multi,
            label: &labels[idx],
        };
        let more = match kind {
            // A symlinked file named as a root is read only when links are
            // followed.
            RootKind::File { is_symlink, .. } if is_symlink && !opts.follow_links => true,
            RootKind::File { meta, is_symlink } => found.offer(scope.file_root(&meta, is_symlink)),
            RootKind::Dir => walk_dir(&scope, opts, include.as_ref(), &exclude, &mut found)?,
        };
        if !more {
            break;
        }
    }
    Ok(found.finish())
}

/// What a root names.
enum RootKind {
    /// A file named as a root, with its metadata (its target's, when the
    /// root is a symlink) and whether the name is a symlink.
    File {
        meta: fs::Metadata,
        is_symlink: bool,
    },
    Dir,
}

impl RootKind {
    fn of(root: &Path) -> Result<Self, Error> {
        if !root.exists() {
            return Err(Error::path(root, "does not exist"));
        }
        let meta = fs::symlink_metadata(root).map_err(|err| Error::path(root, err.to_string()))?;
        let is_symlink = meta.file_type().is_symlink();
        let meta = if is_symlink {
            fs::metadata(root).map_err(|err| Error::path(root, err.to_string()))?
        } else {
            meta
        };
        if meta.is_file() {
            Ok(Self::File { meta, is_symlink })
        } else if meta.is_dir() {
            Ok(Self::Dir)
        } else {
            Err(Error::path(root, "not a file or directory"))
        }
    }
}

/// Files the walk keeps, in the order it finds them.
///
/// Each file found must pass the selection, the skip paths, and the
/// budgets, in that order: a file left out spends no budget.
struct Found<'a> {
    /// Selected names; `None` selects every file.
    selection: Option<HashSet<&'a str>>,
    skip_paths: Vec<PathBuf>,
    budget: Budget,
    files: Vec<WalkedFile>,
}

impl<'a> Found<'a> {
    fn new(opts: &'a Options) -> Self {
        let selection = match &opts.selection {
            Selection::AllEligible => None,
            Selection::Only(names) => Some(names.iter().map(String::as_str).collect()),
        };
        Self {
            selection,
            skip_paths: normalize_skip_paths(&opts.skip_paths),
            budget: Budget::new(opts),
            files: Vec::new(),
        }
    }

    /// Take the next file the walk found. Returns `false` once a file does
    /// not fit the budgets: the walk stops there.
    fn offer(&mut self, file: WalkedFile) -> bool {
        if !self.is_selected(&file) || self.is_skipped(&file) {
            return true;
        }
        if !self.budget.take(file.size) {
            return false;
        }
        self.files.push(file);
        true
    }

    /// Whether the selection names `file`, by id or by relative path.
    fn is_selected(&self, file: &WalkedFile) -> bool {
        let Some(names) = &self.selection else {
            return true;
        };
        names.contains(file.id.as_str()) || names.contains(file.relative.as_str())
    }

    fn is_skipped(&self, file: &WalkedFile) -> bool {
        !self.skip_paths.is_empty() && is_skipped_path(&file.absolute, &self.skip_paths)
    }

    fn finish(mut self) -> WalkOutcome {
        self.files.sort_by(|a, b| a.id.cmp(&b.id));
        WalkOutcome {
            truncated: self.budget.is_cut(),
            files: self.files,
        }
    }
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

/// One root of a walk and how its files are named.
struct RootScope<'a> {
    root: &'a Path,
    /// `root` made absolute once, rather than asking for the working
    /// directory for every file.
    absolute: PathBuf,
    idx: usize,
    multi: bool,
    label: &'a str,
}

impl RootScope<'_> {
    /// Display path for a file whose path under the root is `rel`.
    fn relative(&self, rel: &str) -> String {
        if self.multi {
            if rel.is_empty() {
                self.label.to_string()
            } else {
                format!("{}/{rel}", self.label)
            }
        } else if rel.is_empty() {
            file_name_label(self.root).unwrap_or_default()
        } else {
            rel.to_string()
        }
    }

    fn id(&self, relative: &str) -> String {
        if self.multi {
            format!("{}:{relative}", self.idx)
        } else {
            relative.to_string()
        }
    }

    /// Absolute path of `path`, found by walking this root.
    fn absolute_of(&self, path: &Path) -> PathBuf {
        match path.strip_prefix(self.root) {
            Ok(rest) if !rest.as_os_str().is_empty() => self.absolute.join(rest),
            Ok(_) => self.absolute.clone(),
            Err(_) => make_absolute(path),
        }
    }

    /// The file this root names, when it names a file.
    fn file_root(&self, meta: &fs::Metadata, is_symlink: bool) -> WalkedFile {
        let relative = self.relative("");
        WalkedFile {
            id: self.id(&relative),
            absolute: self.absolute.clone(),
            relative,
            size: meta.len(),
            is_symlink,
            modified: meta.modified().ok(),
        }
    }
}

/// Walk a directory root in path order, handing each file that passes the
/// filters to `found`. Returns `false` once the budgets stop the walk.
fn walk_dir(
    scope: &RootScope<'_>,
    opts: &Options,
    include: Option<&GlobSet>,
    exclude: &GlobSet,
    found: &mut Found<'_>,
) -> Result<bool, Error> {
    let root = scope.root;
    let mut builder = WalkBuilder::new(root);
    builder
        .hidden(!opts.hidden)
        .git_ignore(opts.gitignore)
        .git_global(opts.gitignore)
        .git_exclude(opts.gitignore)
        .ignore(opts.gitignore)
        .follow_links(opts.follow_links)
        .sort_by_file_name(|a, b| a.cmp(b))
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

    for result in builder.build() {
        let Ok(entry) = result else {
            continue;
        };
        if entry.file_type().is_none_or(|ft| !ft.is_file()) {
            continue;
        }
        let is_symlink = entry.path_is_symlink();
        if is_symlink && !opts.follow_links {
            continue;
        }
        let rel = rel_under(root, entry.path());
        if rel.split('/').any(|part| part == ".git") {
            continue;
        }
        let relative = scope.relative(&rel);
        if !keep_for_walk(&relative, include, exclude, opts.follow_archives) {
            continue;
        }
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let file = WalkedFile {
            id: scope.id(&relative),
            absolute: scope.absolute_of(entry.path()),
            relative,
            size: meta.len(),
            is_symlink,
            modified: meta.modified().ok(),
        };
        if !found.offer(file) {
            return Ok(false);
        }
    }
    Ok(true)
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

fn looks_like_archive(relative: &str) -> bool {
    let n = relative.to_ascii_lowercase();
    n.ends_with(".zip") || n.ends_with(".tar") || n.ends_with(".tgz") || n.ends_with(".tar.gz")
}

/// `path` under `root`, with `/` separators and no `.` parts.
fn rel_under(root: &Path, path: &Path) -> String {
    let stripped = path.strip_prefix(root).unwrap_or(path);
    normalize_rel(&stripped.to_string_lossy())
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

/// The last normal component of `path`, if it has one.
fn file_name_label(path: &Path) -> Option<String> {
    path.file_name()
        .map(|name| name.to_string_lossy().replace('\\', "/"))
        .filter(|name| !name.is_empty())
}

fn root_label(root: &Path) -> String {
    root.file_name()
        .map(|name| name.to_string_lossy().replace('\\', "/"))
        .filter(|name| !name.is_empty() && name != ".")
        .unwrap_or_else(|| "root".to_string())
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
    fn test_collect_with_hidden_still_skips_generated_dirs() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("app/page.tsx"), b"export {}\n");
        write(&dir.path().join(".next/server/page.js"), b"export {}\n");
        write(&dir.path().join("out/index.html"), b"<p>x</p>\n");
        write(
            &dir.path().join("toolchains/sdk/a.f90"),
            b"subroutine a\nend\n",
        );
        let mut opts = opts_for(dir.path().to_path_buf());
        opts.hidden = true;
        opts.gitignore = false;
        let files = collect(&opts).unwrap();
        let rels: Vec<&str> = files.iter().map(|f| f.relative.as_str()).collect();
        assert!(rels.contains(&"app/page.tsx"), "{rels:?}");
        assert!(!rels.iter().any(|r| r.contains(".next")), "{rels:?}");
        assert!(!rels.iter().any(|r| r.starts_with("out/")), "{rels:?}");
        assert!(!rels.iter().any(|r| r.contains("toolchains")), "{rels:?}");
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
        write(&dir.path().join("arrays/params.npz"), b"PK\x03\x04npy");
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
        assert!(rels.contains(&"arrays/params.npz"));
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
    fn test_collect_detailed_with_no_entry_cap_keeps_every_file() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("a.rs"), b"fn a() {}\n");
        write(&dir.path().join("b.rs"), b"fn b() {}\n");
        write(&dir.path().join("c.rs"), b"fn c() {}\n");
        let opts = Options {
            roots: vec![dir.path().to_path_buf()],
            max_entries: 0,
            exclude: Vec::new(),
            gitignore: false,
            ..Options::default()
        };
        let outcome = collect_detailed(&opts).unwrap();
        assert_eq!(outcome.files.len(), 3);
        assert!(!outcome.truncated);
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

    fn rels(files: &[WalkedFile]) -> Vec<&str> {
        files.iter().map(|f| f.relative.as_str()).collect()
    }

    #[test]
    fn test_collect_detailed_with_files_over_max_file_size_keeps_every_small_file() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..40 {
            write(&dir.path().join(format!("src/f{i:02}.rs")), b"fn f() {}\n");
        }
        // Sparse files: 1.8 GiB on paper, no disk space. Each is over the
        // per-file cap, so none is ever read.
        fs::create_dir_all(dir.path().join("data")).unwrap();
        for i in 0..3 {
            let file = fs::File::create(dir.path().join(format!("data/w{i}.safetensors")));
            file.unwrap().set_len(600 * 1024 * 1024).unwrap();
        }
        let mut seen = Vec::new();
        for jobs in [1, 2, 8, 0] {
            let opts = Options {
                roots: vec![dir.path().to_path_buf()],
                jobs,
                ..Options::default()
            };
            let outcome = collect_detailed(&opts).unwrap();
            assert!(!outcome.truncated, "jobs={jobs}");
            assert_eq!(outcome.files.len(), 43, "jobs={jobs}");
            seen.push(outcome.files);
        }
        assert!(seen.windows(2).all(|w| w[0] == w[1]));
    }

    #[test]
    fn test_collect_detailed_with_byte_budget_returns_same_prefix_for_any_jobs() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..40 {
            write(&dir.path().join(format!("f{i:02}.txt")), &[b'x'; 100]);
        }
        let expected: Vec<String> = (0..10).map(|i| format!("f{i:02}.txt")).collect();
        for jobs in [1, 3, 8, 1, 8] {
            let opts = Options {
                roots: vec![dir.path().to_path_buf()],
                max_total_bytes: 1000,
                jobs,
                ..Options::default()
            };
            let outcome = collect_detailed(&opts).unwrap();
            assert!(outcome.truncated, "jobs={jobs}");
            assert_eq!(rels(&outcome.files), expected, "jobs={jobs}");
        }
    }

    #[test]
    fn test_collect_detailed_with_max_entries_returns_first_paths() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["d.rs", "b.rs", "a.rs", "c.rs"] {
            write(&dir.path().join(name), b"fn x() {}\n");
        }
        for jobs in [1, 8] {
            let opts = Options {
                roots: vec![dir.path().to_path_buf()],
                max_entries: 2,
                jobs,
                ..Options::default()
            };
            let outcome = collect_detailed(&opts).unwrap();
            assert!(outcome.truncated);
            assert_eq!(rels(&outcome.files), ["a.rs", "b.rs"], "jobs={jobs}");
        }
    }

    #[test]
    fn test_collect_detailed_with_selection_spends_budget_on_selected_files_only() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("a_big.txt"), &[b'x'; 900]);
        write(&dir.path().join("b_small.txt"), &[b'y'; 50]);
        let opts = Options {
            roots: vec![dir.path().to_path_buf()],
            max_total_bytes: 100,
            selection: crate::config::Selection::Only(vec!["b_small.txt".into()]),
            ..Options::default()
        };
        let outcome = collect_detailed(&opts).unwrap();
        assert!(!outcome.truncated);
        assert_eq!(rels(&outcome.files), ["b_small.txt"]);
    }

    #[test]
    fn test_collect_detailed_with_max_entries_keeps_first_file_in_depth_first_order() {
        let dir = tempfile::tempdir().unwrap();
        // As strings `a-c.txt` < `a.txt` < `a/b.txt`, but a walk visits the
        // directory `a` first: it sorts before both names.
        for name in ["a.txt", "a-c.txt", "a/b.txt"] {
            write(&dir.path().join(name), b"x\n");
        }
        for (max_entries, want) in [(1, &["a/b.txt"][..]), (2, &["a-c.txt", "a/b.txt"][..])] {
            let outcome = collect_detailed(&Options {
                roots: vec![dir.path().to_path_buf()],
                max_entries,
                ..Options::default()
            })
            .unwrap();
            assert!(outcome.truncated);
            assert_eq!(rels(&outcome.files), want, "max_entries={max_entries}");
        }
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
