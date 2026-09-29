use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::SystemTime;

use ignore::WalkBuilder;
use ignore::overrides::OverrideBuilder;

use crate::config::{Budget, Options, Selection};
use crate::error::Error;
use crate::pack::ScopedGlobs;

/// A file discovered under the pack roots.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalkedFile {
    /// Unique within one walk. Independent of display [`Self::relative`].
    pub id: String,
    pub absolute: PathBuf,
    /// `/` separators, no leading `./`. With several roots, the root's
    /// label comes first.
    pub relative: String,
    /// Path under its own root, without the multi-root label. A file named
    /// as a root is its own name here.
    pub root_relative: String,
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
    let globs = ScopedGlobs::new(opts)?;
    let labels = root_labels(&opts.roots);
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
            RootKind::File { meta, is_symlink } => found.offer(scope.file_root(&meta, is_symlink)),
            RootKind::Dir => walk_dir(&scope, opts, &globs, &mut found)?,
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
/// Each file found gets an id no earlier file has, then must pass the
/// selection, the skip paths, and the budgets, in that order: ids never
/// depend on what is selected, and a file left out spends no budget.
struct Found<'a> {
    /// Selected names; `None` selects every file.
    selection: Option<HashSet<&'a str>>,
    skip_paths: Vec<PathBuf>,
    budget: Budget,
    ids: HashSet<String>,
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
            ids: HashSet::new(),
            files: Vec::new(),
        }
    }

    /// Take the next file the walk found. Returns `false` once a file does
    /// not fit the budgets: the walk stops there.
    fn offer(&mut self, mut file: WalkedFile) -> bool {
        file.id = self.unique_id(std::mem::take(&mut file.id));
        if !self.is_selected(&file) || self.is_skipped(&file) {
            return true;
        }
        if !self.budget.take(file.size) {
            return false;
        }
        self.files.push(file);
        true
    }

    /// `id`, or when an earlier file has it, `id#2`, `id#3`, and so on.
    ///
    /// Names that are the same once turned into text (bytes that are not
    /// UTF-8 against a literal `\xNN`, say) would otherwise share an id.
    fn unique_id(&mut self, id: String) -> String {
        if self.ids.insert(id.clone()) {
            return id;
        }
        let mut n = 2usize;
        loop {
            let candidate = format!("{id}#{n}");
            if self.ids.insert(candidate.clone()) {
                return candidate;
            }
            n += 1;
        }
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
        // A file named on the command line is read even when it is a
        // symlink, the way a symlinked directory root is walked.
        let relative = self.relative("");
        let root_relative = file_name_label(self.root).unwrap_or_else(|| relative.clone());
        WalkedFile {
            id: self.id(&relative),
            absolute: self.absolute.clone(),
            relative,
            root_relative,
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
    globs: &ScopedGlobs,
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

    // Excluded directories are pruned whole. These match the path under the
    // root; the user's globs are also matched against the labelled path, per
    // file, below.
    let prune = opts.exclude_globs();
    if !prune.is_empty() {
        let mut overrides = OverrideBuilder::new(root);
        for pat in &prune {
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
        let unpacking = opts.follow_archives && looks_like_archive(&rel);
        if !globs.keep(&rel, &relative, unpacking) {
            continue;
        }
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let file = WalkedFile {
            id: scope.id(&relative),
            absolute: scope.absolute_of(entry.path()),
            relative,
            root_relative: rel,
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

fn looks_like_archive(relative: &str) -> bool {
    let n = relative.to_ascii_lowercase();
    n.ends_with(".zip") || n.ends_with(".tar") || n.ends_with(".tgz") || n.ends_with(".tar.gz")
}

/// `path` under `root`, as `/`-separated normal components.
///
/// Built from path components rather than by rewriting `\` so a Unix file
/// name that holds a backslash stays one name.
fn rel_under(root: &Path, path: &Path) -> String {
    let stripped = path.strip_prefix(root).unwrap_or(path);
    let parts: Vec<String> = stripped
        .components()
        .filter_map(|component| match component {
            Component::Normal(part) => Some(os_text(part)),
            _ => None,
        })
        .collect();
    parts.join("/")
}

/// A file name as text. On Unix, bytes that are not UTF-8 become `\xNN`
/// escapes. Different names still get different ids (see
/// [`Found::unique_id`]), but a name holding a literal `\xNN` prints like
/// one whose byte was escaped.
fn os_text(name: &OsStr) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        crate::tree::escape_invalid_utf8(name.as_bytes()).into_owned()
    }
    #[cfg(not(unix))]
    {
        name.to_string_lossy().into_owned()
    }
}

/// The last normal component of `path`, if it has one.
fn file_name_label(path: &Path) -> Option<String> {
    path.file_name()
        .map(os_text)
        .filter(|name| !name.is_empty())
}

/// Labels that prefix each root's files when several roots are walked.
///
/// A label is the root's directory name, with `.` and `..` resolved to the
/// directory they name. Labels grow by parent directories until they are
/// prefix-free: none equals another or is a component prefix of one, since
/// `a` and `a/b` would print `b/x.txt` under the first and `x.txt` under the
/// second as the same `a/b/x.txt`. Equal labels all grow; of a label and one
/// that extends it, the shorter grows, or the longer when the shorter has no
/// parent left. A root named twice keeps one label.
pub(crate) fn root_labels(roots: &[PathBuf]) -> Vec<String> {
    let names: Vec<Vec<String>> = roots.iter().map(|root| named_components(root)).collect();
    let mut distinct: Vec<&[String]> = Vec::new();
    let mut slot_of: HashMap<&[String], usize> = HashMap::new();
    let slots: Vec<usize> = names
        .iter()
        .map(|parts| {
            *slot_of.entry(parts.as_slice()).or_insert_with(|| {
                distinct.push(parts.as_slice());
                distinct.len() - 1
            })
        })
        .collect();
    let mut take = vec![1usize; distinct.len()];
    loop {
        let labels: Vec<String> = distinct
            .iter()
            .zip(&take)
            .map(|(parts, &take)| label_of(parts, take))
            .collect();
        let grow = labels_to_grow(&labels, |slot| take[slot] < distinct[slot].len());
        if grow.is_empty() {
            return slots.iter().map(|&slot| labels[slot].clone()).collect();
        }
        for slot in grow {
            take[slot] += 1;
        }
    }
}

/// The last `take` components of `parts`, or `root` for the filesystem root.
fn label_of(parts: &[String], take: usize) -> String {
    if parts.is_empty() {
        return "root".to_string();
    }
    parts[parts.len().saturating_sub(take)..].join("/")
}

/// Which of `labels` must grow by a parent directory, among those that can:
/// one equal to another, one that another extends, and one that extends a
/// label that cannot grow.
fn labels_to_grow(labels: &[String], can_grow: impl Fn(usize) -> bool) -> Vec<usize> {
    let mut count: HashMap<&str, usize> = HashMap::new();
    let mut extended: HashSet<&str> = HashSet::new();
    let mut stuck: HashSet<&str> = HashSet::new();
    for (slot, label) in labels.iter().enumerate() {
        *count.entry(label.as_str()).or_default() += 1;
        extended.extend(proper_prefixes(label));
        if !can_grow(slot) {
            stuck.insert(label.as_str());
        }
    }
    labels
        .iter()
        .enumerate()
        .filter(|&(slot, label)| {
            can_grow(slot)
                && (count[label.as_str()] > 1
                    || extended.contains(label.as_str())
                    || proper_prefixes(label).any(|prefix| stuck.contains(prefix)))
        })
        .map(|(slot, _)| slot)
        .collect()
}

/// The component prefixes of `label` short of itself: `a` and `a/b` for
/// `a/b/c`.
fn proper_prefixes(label: &str) -> impl Iterator<Item = &str> {
    label.match_indices('/').map(move |(end, _)| &label[..end])
}

/// Normal components of `path` made absolute, with `.` and `..` resolved
/// by name. Symlinks are kept as written, so a label shows the name typed.
fn named_components(path: &Path) -> Vec<String> {
    let absolute = make_absolute(path);
    let mut parts: Vec<String> = Vec::new();
    for component in absolute.components() {
        match component {
            Component::Normal(part) => parts.push(os_text(part)),
            Component::ParentDir => {
                parts.pop();
            }
            Component::CurDir | Component::RootDir | Component::Prefix(_) => {}
        }
    }
    parts
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
            default_excludes: false,
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
            default_excludes: false,
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
            default_excludes: false,
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
        opts.default_excludes = false;
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
        opts.default_excludes = false;
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
    fn test_collect_with_roots_named_like_excluded_dirs_keeps_their_files() {
        let dir = tempfile::tempdir().unwrap();
        let runs = dir.path().join("runs");
        let notes = dir.path().join("notes");
        write(&runs.join("log.txt"), b"loss 0.1\n");
        write(&runs.join("build/out.txt"), b"generated\n");
        write(&notes.join("a.md"), b"# a\n");
        let opts = Options {
            roots: vec![runs, notes],
            ..Options::default()
        };
        let files = collect(&opts).unwrap();
        assert_eq!(rels(&files), ["runs/log.txt", "notes/a.md"]);
        let log = files.iter().find(|f| f.relative == "runs/log.txt").unwrap();
        assert_eq!(log.root_relative, "log.txt");
    }

    /// Roots `app` and `lib`, each with `src/`, and `app` with `secrets/`.
    fn app_and_lib(dir: &Path) -> Vec<PathBuf> {
        let app = dir.join("app");
        let lib = dir.join("lib");
        write(&app.join("src/a.rs"), b"fn a() {}\n");
        write(&app.join("secrets/prod.txt"), b"key\n");
        write(&lib.join("src/l.rs"), b"fn l() {}\n");
        write(&lib.join("secrets/dev.txt"), b"key\n");
        vec![app, lib]
    }

    #[test]
    fn test_collect_with_multiple_roots_and_label_scoped_exclude_drops_those_files() {
        let dir = tempfile::tempdir().unwrap();
        let opts = Options {
            roots: app_and_lib(dir.path()),
            exclude: vec!["app/secrets/**".into()],
            ..Options::default()
        };
        assert_eq!(
            rels(&collect(&opts).unwrap()),
            ["app/src/a.rs", "lib/secrets/dev.txt", "lib/src/l.rs"]
        );
    }

    #[test]
    fn test_collect_with_multiple_roots_and_label_scoped_include_keeps_those_files() {
        let dir = tempfile::tempdir().unwrap();
        let opts = Options {
            roots: app_and_lib(dir.path()),
            include: vec!["app/src/**".into()],
            ..Options::default()
        };
        assert_eq!(rels(&collect(&opts).unwrap()), ["app/src/a.rs"]);
    }

    #[test]
    fn test_collect_with_multiple_roots_and_root_relative_exclude_drops_those_files() {
        let dir = tempfile::tempdir().unwrap();
        let opts = Options {
            roots: app_and_lib(dir.path()),
            exclude: vec!["secrets/**".into()],
            ..Options::default()
        };
        assert_eq!(
            rels(&collect(&opts).unwrap()),
            ["app/src/a.rs", "lib/src/l.rs"]
        );
    }

    #[test]
    fn test_collect_with_user_exclude_keeps_default_excludes() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("src/a.rs"), b"fn a() {}\n");
        write(&dir.path().join("notes.log"), b"log\n");
        write(&dir.path().join("node_modules/x/i.js"), b"x\n");
        let opts = Options {
            roots: vec![dir.path().to_path_buf()],
            exclude: vec!["*.log".into()],
            ..Options::default()
        };
        assert_eq!(rels(&collect(&opts).unwrap()), ["src/a.rs"]);
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
    fn test_root_labels_with_shared_names_and_dot_roots_returns_distinct_labels() {
        let labels = root_labels(&[PathBuf::from("/tmp/a/src"), PathBuf::from("/tmp/b/src")]);
        assert_eq!(labels, ["a/src", "b/src"]);
        let labels = root_labels(&[PathBuf::from("/x/one"), PathBuf::from("/y/two")]);
        assert_eq!(labels, ["one", "two"]);
        let labels = root_labels(&[
            PathBuf::from("/x/p/.."),
            PathBuf::from("/"),
            "/x/q/.".into(),
        ]);
        assert_eq!(labels, ["x", "root", "q"]);

        let cwd = std::env::current_dir().unwrap();
        let labels = root_labels(&[PathBuf::from("."), PathBuf::from("..")]);
        let name = |p: &Path| p.file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!(labels, [name(&cwd), name(cwd.parent().unwrap())]);
    }

    #[test]
    fn test_root_labels_with_label_nested_in_another_returns_prefix_free_labels() {
        let labels = root_labels(&[
            PathBuf::from("/p/a"),
            PathBuf::from("/q/a/b"),
            PathBuf::from("/r/b"),
        ]);
        assert_eq!(labels, ["p/a", "a/b", "r/b"]);
        // `a` has no parent left to add, so the label extending it grows.
        let labels = root_labels(&[PathBuf::from("/a"), PathBuf::from("/x/a/b"), "/y/b".into()]);
        assert_eq!(labels, ["a", "x/a/b", "y/b"]);
        // One directory named twice keeps one label.
        let labels = root_labels(&[PathBuf::from("/s/src"), PathBuf::from("/s/src")]);
        assert_eq!(labels, ["src", "src"]);
    }

    #[test]
    fn test_collect_with_roots_whose_labels_nest_returns_distinct_paths() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("p/a");
        let second = dir.path().join("q/a/b");
        let third = dir.path().join("r/b");
        write(&first.join("b/x.txt"), b"one\n");
        write(&second.join("x.txt"), b"two\n");
        write(&third.join("y.txt"), b"three\n");
        let files = collect(&Options {
            roots: vec![first, second, third],
            ..Options::default()
        })
        .unwrap();
        assert_eq!(rels(&files), ["p/a/b/x.txt", "a/b/x.txt", "r/b/y.txt"]);
    }

    #[cfg(unix)]
    #[test]
    fn test_collect_with_symlink_file_root_returns_the_file() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("real.rs"), b"fn real() {}\n");
        let link = dir.path().join("link.rs");
        std::os::unix::fs::symlink(dir.path().join("real.rs"), &link).unwrap();
        let files = collect(&opts_for(link)).unwrap();
        assert_eq!(rels(&files), ["link.rs"]);
        assert!(files[0].is_symlink);
        assert_eq!(files[0].size, 13);
    }

    fn walked(relative: &str) -> WalkedFile {
        WalkedFile {
            id: relative.into(),
            absolute: PathBuf::from(relative),
            relative: relative.into(),
            root_relative: relative.into(),
            size: 0,
            is_symlink: false,
            modified: None,
        }
    }

    #[test]
    fn test_found_with_shared_ids_returns_unique_ids_in_walk_order() {
        let opts = Options::default();
        let mut found = Found::new(&opts);
        for relative in ["a.txt", "a.txt", "a.txt#2", "a.txt", "b.txt"] {
            assert!(found.offer(walked(relative)));
        }
        let files = found.finish().files;
        let ids: Vec<&str> = files.iter().map(|f| f.id.as_str()).collect();
        assert_eq!(ids, ["a.txt", "a.txt#2", "a.txt#2#2", "a.txt#3", "b.txt"]);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_collect_with_non_utf8_names_returns_distinct_paths_and_ids() {
        use std::os::unix::ffi::OsStrExt;
        let dir = tempfile::tempdir().unwrap();
        for name in [
            &b"a\xff.txt"[..],
            &b"a\xfe.txt"[..],
            &b"a\xef\xbf\xbd.txt"[..],
        ] {
            write(&dir.path().join(OsStr::from_bytes(name)), b"x\n");
        }
        let files = collect(&opts_for(dir.path().to_path_buf())).unwrap();
        let mut rels = rels(&files);
        rels.sort_unstable();
        assert_eq!(rels, ["a\\xfe.txt", "a\\xff.txt", "a\u{fffd}.txt"]);
        let ids: HashSet<&str> = files.iter().map(|f| f.id.as_str()).collect();
        assert_eq!(ids.len(), 3);
    }

    #[cfg(unix)]
    #[test]
    fn test_collect_with_backslash_in_file_name_keeps_one_segment() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("a\\b.txt"), b"one file\n");
        let files = collect(&opts_for(dir.path().to_path_buf())).unwrap();
        assert_eq!(rels(&files), ["a\\b.txt"]);
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
