use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fs;
use std::io;
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
    /// Device and inode when found (Unix only), so the walk can leave out a
    /// file it must skip whatever path names it.
    pub identity: Option<(u64, u64)>,
}

/// Walked files plus whether discovery stopped at a budget.
#[derive(Debug, Clone)]
pub struct WalkOutcome {
    pub files: Vec<WalkedFile>,
    pub truncated: bool,
    /// Paths the walk could not read, such as a directory without read
    /// permission, whose files are therefore missing.
    pub warnings: WalkWarnings,
}

/// Most walk warnings kept word for word; the rest are only counted.
pub const MAX_WALK_WARNINGS: usize = 1000;

/// Problems met while walking, in walk order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WalkWarnings {
    /// The first [`MAX_WALK_WARNINGS`] messages. The walk visits paths in
    /// the same order on every run, so the same ones are kept.
    pub messages: Vec<String>,
    /// Every warning, kept or not.
    pub total: usize,
}

impl WalkWarnings {
    fn push(&mut self, message: String) {
        self.total += 1;
        if self.messages.len() < MAX_WALK_WARNINGS {
            self.messages.push(message);
        }
    }
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
/// selection, the skip set, and the budgets, in that order: ids never depend
/// on what is selected, and a file left out spends no budget.
struct Found<'a> {
    /// Selected names; `None` selects every file.
    selection: Option<HashSet<&'a str>>,
    skip: SkipSet,
    budget: Budget,
    ids: HashSet<String>,
    files: Vec<WalkedFile>,
    warnings: WalkWarnings,
}

impl<'a> Found<'a> {
    fn new(opts: &'a Options) -> Self {
        let selection = match &opts.selection {
            Selection::AllEligible => None,
            Selection::Only(names) => Some(names.iter().map(String::as_str).collect()),
        };
        Self {
            selection,
            skip: SkipSet::new(opts),
            budget: Budget::new(opts),
            ids: HashSet::new(),
            files: Vec::new(),
            warnings: WalkWarnings::default(),
        }
    }

    /// Take the next file the walk found. Returns `false` once a file does
    /// not fit the budgets: the walk stops there.
    fn offer(&mut self, mut file: WalkedFile) -> bool {
        file.id = self.unique_id(std::mem::take(&mut file.id));
        if !self.is_selected(&file) || self.skip.contains(&file) {
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

    /// Whether the selection names `file`. A name picks the file whose id it
    /// is; a name that no file found so far has as its id falls back to the
    /// relative path. The first file found at a path keeps that path as its
    /// id, so a later file whose name prints alike is never picked by it.
    fn is_selected(&self, file: &WalkedFile) -> bool {
        let Some(names) = &self.selection else {
            return true;
        };
        names.contains(file.id.as_str())
            || (names.contains(file.relative.as_str()) && !self.ids.contains(&file.relative))
    }

    fn finish(mut self) -> WalkOutcome {
        self.files.sort_by(|a, b| a.id.cmp(&b.id));
        WalkOutcome {
            truncated: self.budget.is_cut(),
            files: self.files,
            warnings: self.warnings,
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

/// Files the walk must leave out, such as the dump being written.
///
/// Matched by path, and on Unix by device and inode: those of each skip
/// path when the set is made, and those the caller took from open
/// descriptors ([`Options::skip_identities`]), which name a file where a
/// path such as `/dev/stdout` cannot. A walked file carries its own
/// identity, so a match costs no system call.
struct SkipSet {
    paths: Vec<PathBuf>,
    ids: Vec<(u64, u64)>,
}

impl SkipSet {
    fn new(opts: &Options) -> Self {
        let paths = normalize_skip_paths(&opts.skip_paths);
        let mut ids: Vec<(u64, u64)> = paths
            .iter()
            .filter_map(|path| fs::metadata(path).ok())
            .filter_map(|meta| file_identity(&meta))
            .collect();
        ids.extend_from_slice(&opts.skip_identities);
        Self { paths, ids }
    }

    fn contains(&self, file: &WalkedFile) -> bool {
        self.paths.contains(&file.absolute)
            || file.identity.is_some_and(|id| self.ids.contains(&id))
            || self.names_another_way(&file.absolute)
    }

    /// Without device and inode numbers, a file that a skip path names
    /// another way is found by canonicalizing its path.
    #[cfg(not(unix))]
    fn names_another_way(&self, path: &Path) -> bool {
        !self.paths.is_empty()
            && path
                .canonicalize()
                .is_ok_and(|canonical| self.paths.contains(&canonical))
    }

    #[cfg(unix)]
    fn names_another_way(&self, _path: &Path) -> bool {
        false
    }
}

/// Device and inode of a file, where the platform has them.
#[cfg(unix)]
pub(crate) fn file_identity(meta: &fs::Metadata) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    Some((meta.dev(), meta.ino()))
}

#[cfg(not(unix))]
pub(crate) fn file_identity(_meta: &fs::Metadata) -> Option<(u64, u64)> {
    None
}

/// Open `path` for reading without blocking and, unless `follow`, without
/// following a symlink.
///
/// A path that was a regular file at scan time may since have become a
/// FIFO, a device, or a symlink to a file outside the folder. On Unix the
/// open never blocks (`O_NONBLOCK`: a FIFO does not wait for a writer) and
/// fails on a symlink (`O_NOFOLLOW`) unless the scan found one there and
/// `follow` says so. Check the opened file's metadata rather than the
/// path's: a swap after the open cannot change what is read.
pub(crate) fn open_for_read(path: &Path, follow: bool) -> io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let nofollow = if follow { 0 } else { libc::O_NOFOLLOW };
        options.custom_flags(libc::O_NONBLOCK | nofollow);
    }
    // Without those flags, check the type first, so a path that is no
    // longer a regular file is not opened at all.
    #[cfg(not(unix))]
    {
        let _ = follow;
        if !fs::metadata(path)?.is_file() {
            return Err(not_regular_file());
        }
    }
    options.open(path)
}

/// [`open_for_read`], refusing anything but a regular file.
pub(crate) fn open_regular_file(path: &Path, follow: bool) -> io::Result<fs::File> {
    let file = open_for_read(path, follow)?;
    if !file.metadata()?.is_file() {
        return Err(not_regular_file());
    }
    Ok(file)
}

fn not_regular_file() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "not a regular file; it changed after the scan",
    )
}

/// How the path to a scanned file changed, as [`open_beneath`] found it.
#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PathChange {
    /// The file, or a folder on its path, is now a symlink.
    Symlink,
    /// The file, or a folder on its path, is gone.
    Gone,
    /// A folder on its path is no longer a folder.
    NotFolder,
}

/// Open `path` for reading without following a symlink in its last `depth`
/// components: the steps from the scanned root down to the file.
///
/// `O_NOFOLLOW` on a whole path guards only its last component, so a folder
/// on the way that became a symlink to somewhere outside the root would
/// still be followed. Instead the root is opened, each folder below it is
/// opened from the one above with `O_NOFOLLOW | O_DIRECTORY`, and the file
/// itself with `O_NOFOLLOW | O_NONBLOCK`, so a FIFO does not wait for a
/// writer. A step that is now a symlink, gone, or no longer a folder comes
/// back as that [`PathChange`]. The caller checks the opened file's type.
#[cfg(unix)]
pub(crate) fn open_beneath(path: &Path, depth: usize) -> io::Result<Result<fs::File, PathChange>> {
    use std::os::fd::OwnedFd;
    use std::os::unix::fs::OpenOptionsExt;

    let mut root = path.to_path_buf();
    let mut steps = Vec::with_capacity(depth);
    for _ in 0..depth {
        let name = root.file_name().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "path is shorter than its depth",
            )
        })?;
        steps.push(c_name(name)?);
        root.pop();
    }
    steps.reverse();
    let Some((file, folders)) = steps.split_last() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "no file to open",
        ));
    };
    let opened = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY)
        .open(&root);
    let mut dir = match opened {
        Ok(dir) => OwnedFd::from(dir),
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Err(PathChange::Gone)),
        Err(err) if err.kind() == io::ErrorKind::NotADirectory => {
            return Ok(Err(PathChange::NotFolder));
        }
        Err(err) => return Err(err),
    };
    for folder in folders {
        match open_at(&dir, folder, libc::O_DIRECTORY) {
            Ok(next) => dir = next,
            Err(err) => return step_change(&dir, folder, true, err).map(Err),
        }
    }
    match open_at(&dir, file, libc::O_NONBLOCK) {
        Ok(fd) => Ok(Ok(fs::File::from(fd))),
        Err(err) => step_change(&dir, file, false, err).map(Err),
    }
}

#[cfg(unix)]
fn c_name(name: &OsStr) -> io::Result<std::ffi::CString> {
    use std::os::unix::ffi::OsStrExt;
    std::ffi::CString::new(name.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "file name holds a NUL byte"))
}

/// `openat(dir, name)` for reading, never following a symlink at `name`.
#[cfg(unix)]
fn open_at(
    dir: &std::os::fd::OwnedFd,
    name: &std::ffi::CStr,
    flags: libc::c_int,
) -> io::Result<std::os::fd::OwnedFd> {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    let flags = flags | libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    // SAFETY: `dir` is an open descriptor and `name` a NUL-terminated
    // string; both outlive the call.
    let fd = unsafe { libc::openat(dir.as_raw_fd(), name.as_ptr(), flags) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `openat` just returned `fd`, and nothing else owns it.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// How the step `name` in `dir`, which failed to open with `err`, changed:
/// it is a symlink, it is gone, or a folder step is no longer a folder.
/// Any other failure comes back as `err`.
#[cfg(unix)]
fn step_change(
    dir: &std::os::fd::OwnedFd,
    name: &std::ffi::CStr,
    folder: bool,
    err: io::Error,
) -> io::Result<PathChange> {
    use std::os::fd::AsRawFd;
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: `dir` is open, `name` is NUL-terminated, and `stat` points to
    // writable memory the size of a `stat`.
    let status = unsafe {
        libc::fstatat(
            dir.as_raw_fd(),
            name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if status != 0 {
        let gone = io::Error::last_os_error().kind() == io::ErrorKind::NotFound;
        return if gone { Ok(PathChange::Gone) } else { Err(err) };
    }
    // SAFETY: `fstatat` succeeded, so it filled in `stat`.
    let kind = unsafe { stat.assume_init() }.st_mode & libc::S_IFMT;
    if kind == libc::S_IFLNK {
        Ok(PathChange::Symlink)
    } else if folder && kind != libc::S_IFDIR {
        Ok(PathChange::NotFolder)
    } else {
        Err(err)
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
            identity: file_identity(meta),
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
        let entry = match result {
            Ok(entry) => entry,
            Err(err) => {
                // An unreadable directory drops every file under it; say so
                // rather than leave a silent gap.
                found.warnings.push(err.to_string());
                continue;
            }
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
            identity: file_identity(&meta),
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

    #[cfg(unix)]
    #[test]
    fn test_collect_detailed_with_max_entries_never_walks_past_the_cut() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("a.rs"), b"fn a() {}\n");
        write(&dir.path().join("b.rs"), b"fn b() {}\n");
        let locked = dir.path().join("zz");
        write(&locked.join("c.rs"), b"fn c() {}\n");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
        let privileged = fs::read_dir(&locked).is_ok();
        let outcome = collect_detailed(&Options {
            roots: vec![dir.path().to_path_buf()],
            max_entries: 1,
            ..Options::default()
        });
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        if privileged {
            // Running as root: nothing is unreadable, so there is nothing to test.
            return;
        }
        let outcome = outcome.unwrap();
        assert!(outcome.truncated);
        assert_eq!(rels(&outcome.files), ["a.rs"]);
        assert_eq!(outcome.warnings.total, 0, "{:?}", outcome.warnings);
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
    fn test_collect_detailed_with_more_warnings_than_kept_returns_first_in_path_order() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("a.rs"), b"fn a() {}\n");
        let locked: Vec<PathBuf> = (0..MAX_WALK_WARNINGS + 5)
            .map(|i| dir.path().join(format!("d{i:04}")))
            .collect();
        for path in &locked {
            write(&path.join("x.rs"), b"fn x() {}\n");
            fs::set_permissions(path, fs::Permissions::from_mode(0o000)).unwrap();
        }
        let privileged = fs::read_dir(&locked[0]).is_ok();
        let outcomes: Vec<_> = [1, 8]
            .into_iter()
            .map(|jobs| {
                collect_detailed(&Options {
                    roots: vec![dir.path().to_path_buf()],
                    jobs,
                    ..Options::default()
                })
            })
            .collect();
        for path in &locked {
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        if privileged {
            // Running as root: nothing is unreadable, so there is nothing to test.
            return;
        }
        let warnings: Vec<WalkWarnings> = outcomes
            .into_iter()
            .map(|outcome| outcome.unwrap().warnings)
            .collect();
        assert_eq!(warnings[0], warnings[1]);
        let kept = &warnings[0];
        assert_eq!(kept.total, MAX_WALK_WARNINGS + 5);
        assert_eq!(kept.messages.len(), MAX_WALK_WARNINGS);
        assert!(kept.messages[0].contains("d0000"), "{}", kept.messages[0]);
        let last = &kept.messages[MAX_WALK_WARNINGS - 1];
        assert!(
            last.contains(&format!("d{:04}", MAX_WALK_WARNINGS - 1)),
            "{last}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_open_regular_file_with_symlink_for_non_symlink_entry_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real.txt");
        write(&real, b"real\n");
        let link = dir.path().join("link.txt");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert!(open_regular_file(&link, false).is_err());
        assert!(open_regular_file(&link, true).is_ok());
        assert!(open_regular_file(&real, false).is_ok());
    }

    /// What `open_beneath` gives for `path`, read to text when it opens.
    #[cfg(unix)]
    fn beneath(path: &Path, depth: usize) -> Result<String, PathChange> {
        use std::io::Read;
        open_beneath(path, depth).unwrap().map(|mut file| {
            let mut text = String::new();
            file.read_to_string(&mut text).unwrap();
            text
        })
    }

    #[cfg(unix)]
    #[test]
    fn test_open_beneath_with_folder_swapped_for_symlink_returns_symlink_change() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        write(&dir.path().join("sub/b.txt"), b"inside!\n");
        write(&outside.path().join("b.txt"), b"outside\n");
        let path = dir.path().join("sub/b.txt");
        assert_eq!(beneath(&path, 2), Ok("inside!\n".to_string()));
        fs::rename(dir.path().join("sub"), dir.path().join("sub.old")).unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("sub")).unwrap();
        assert_eq!(beneath(&path, 2), Err(PathChange::Symlink));
        // Only the last `depth` steps are checked; the path to the root is
        // the caller's to trust.
        assert_eq!(beneath(&path, 1), Ok("outside\n".to_string()));
    }

    #[cfg(unix)]
    #[test]
    fn test_open_beneath_with_file_swapped_for_symlink_returns_symlink_change() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        write(&outside.path().join("secret.txt"), b"private\n");
        let path = dir.path().join("a/b/c.txt");
        write(&path, b"scanned\n");
        fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink(outside.path().join("secret.txt"), &path).unwrap();
        assert_eq!(beneath(&path, 3), Err(PathChange::Symlink));
    }

    #[cfg(unix)]
    #[test]
    fn test_open_beneath_with_missing_step_or_file_in_place_of_folder_returns_change() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a/b/c.txt");
        write(&path, b"x\n");
        fs::remove_file(&path).unwrap();
        assert_eq!(beneath(&path, 3), Err(PathChange::Gone));
        fs::remove_dir(dir.path().join("a/b")).unwrap();
        assert_eq!(beneath(&path, 3), Err(PathChange::Gone));
        write(&dir.path().join("a/b"), b"a file now\n");
        assert_eq!(beneath(&path, 3), Err(PathChange::NotFolder));
    }

    #[cfg(unix)]
    #[test]
    fn test_open_beneath_with_fifo_returns_it_without_blocking() {
        use std::os::unix::fs::FileTypeExt;
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("sub/pipe");
        fs::create_dir_all(dir.path().join("sub")).unwrap();
        let made = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap();
        assert!(made.success());
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let opened = open_beneath(&fifo, 2).unwrap().unwrap();
            let _ = tx.send(opened.metadata().unwrap().file_type().is_fifo());
        });
        let is_fifo = rx
            .recv_timeout(std::time::Duration::from_secs(20))
            .expect("opening a FIFO must not wait for a writer");
        assert!(is_fifo);
    }

    #[cfg(unix)]
    #[test]
    fn test_open_regular_file_with_fifo_after_type_check_returns_without_blocking() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("pipe");
        let made = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap();
        assert!(made.success());
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(
                open_regular_file(&fifo, false)
                    .map(|_| ())
                    .map_err(|e| e.kind()),
            );
        });
        let opened = rx
            .recv_timeout(std::time::Duration::from_secs(20))
            .expect("opening a FIFO must not wait for a writer");
        assert_eq!(opened, Err(io::ErrorKind::InvalidInput));
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

    #[cfg(unix)]
    #[test]
    fn test_collect_with_skip_path_hard_linked_to_file_skips_it() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("keep.rs"), b"fn keep() {}\n");
        write(&dir.path().join("dump.txt"), b"old dump\n");
        // A hard link has a path of its own that canonicalizes to itself,
        // so only the device and inode can tie it to `dump.txt`.
        let other = tempfile::tempdir().unwrap();
        let alias = other.path().join("alias");
        fs::hard_link(dir.path().join("dump.txt"), &alias).unwrap();
        let opts = Options {
            roots: vec![dir.path().to_path_buf()],
            skip_paths: vec![alias],
            ..Options::default()
        };
        assert_eq!(rels(&collect(&opts).unwrap()), ["keep.rs"]);
    }

    #[cfg(unix)]
    #[test]
    fn test_collect_with_skip_identity_skips_that_file_before_budgets() {
        use std::os::unix::fs::MetadataExt;
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("0dump.txt"), b"");
        write(&dir.path().join("a.rs"), b"fn a() {}\n");
        let meta = fs::metadata(dir.path().join("0dump.txt")).unwrap();
        let outcome = collect_detailed(&Options {
            roots: vec![dir.path().to_path_buf()],
            skip_identities: vec![(meta.dev(), meta.ino())],
            max_entries: 1,
            ..Options::default()
        })
        .unwrap();
        assert_eq!(rels(&outcome.files), ["a.rs"]);
        assert!(!outcome.truncated);
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
            identity: None,
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

    #[test]
    fn test_collect_with_selected_id_of_colliding_name_returns_only_that_file() {
        // Two files whose names print alike, as `a\xff.txt` and a literal
        // `a\xff.txt` do: one relative path, ids `a\xff.txt` and `…#2`.
        for (pick, want) in [
            ("a\\xff.txt", "a\\xff.txt"),
            ("a\\xff.txt#2", "a\\xff.txt#2"),
        ] {
            let opts = Options {
                selection: Selection::Only(vec![pick.to_string()]),
                ..Options::default()
            };
            let mut found = Found::new(&opts);
            assert!(found.offer(walked("a\\xff.txt")));
            assert!(found.offer(walked("a\\xff.txt")));
            let ids: Vec<String> = found.finish().files.into_iter().map(|f| f.id).collect();
            assert_eq!(ids, [want], "{pick}");
        }
        // With several roots a relative path is no id, so it still selects.
        let opts = Options {
            selection: Selection::Only(vec!["app/x.rs".into()]),
            ..Options::default()
        };
        let mut found = Found::new(&opts);
        let mut file = walked("app/x.rs");
        file.id = "0:app/x.rs".into();
        assert!(found.offer(file));
        assert_eq!(found.finish().files.len(), 1);
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

    #[cfg(target_os = "linux")]
    #[test]
    fn test_collect_with_selected_id_of_names_that_print_alike_returns_that_file() {
        use std::os::unix::ffi::OsStrExt;
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join(OsStr::from_bytes(b"a\xff.txt")), b"byte\n");
        write(&dir.path().join("a\\xff.txt"), b"literal\n");
        let all = collect(&opts_for(dir.path().to_path_buf())).unwrap();
        let ids: Vec<String> = all.iter().map(|f| f.id.clone()).collect();
        assert_eq!(ids, ["a\\xff.txt", "a\\xff.txt#2"]);
        assert!(all.iter().all(|f| f.relative == "a\\xff.txt"));
        for id in ids {
            let picked = collect(&Options {
                roots: vec![dir.path().to_path_buf()],
                selection: Selection::Only(vec![id.clone()]),
                ..Options::default()
            })
            .unwrap();
            let picked: Vec<&str> = picked.iter().map(|f| f.id.as_str()).collect();
            assert_eq!(picked, [id.as_str()]);
        }
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
