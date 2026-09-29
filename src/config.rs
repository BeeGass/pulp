use std::cmp::Ordering;
use std::path::PathBuf;

/// How the directory map at the top of the dump is produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TreeMode {
    /// Only paths that were actually pulped.
    #[default]
    Selected,
    /// Selected paths plus skipped placeholders.
    Full,
    /// No directory map.
    None,
}

/// Layout of the concatenated dump.
///
/// CLI aliases: `txt`/`plain` -> [`Plain`], `md`/`markdown` -> [`Markdown`],
/// `xml` -> [`Xml`]. Passing `-o dump.md` selects Markdown when `-f` is omitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OutputFormat {
    #[default]
    Plain,
    Markdown,
    Xml,
}

impl OutputFormat {
    /// Map a file extension or format name (`txt`, `md`, `xml`, `plain`, …).
    #[must_use]
    pub fn from_ext(ext: &str) -> Option<Self> {
        match ext
            .trim()
            .trim_start_matches('.')
            .to_ascii_lowercase()
            .as_str()
        {
            "txt" | "text" | "plain" => Some(Self::Plain),
            "md" | "markdown" => Some(Self::Markdown),
            "xml" => Some(Self::Xml),
            _ => None,
        }
    }

    /// Infer format from an output path's extension (`.txt`, `.md`, `.xml`).
    #[must_use]
    pub fn from_path(path: &std::path::Path) -> Option<Self> {
        path.extension()
            .and_then(|e| e.to_str())
            .and_then(Self::from_ext)
    }

    #[must_use]
    pub fn extension(self) -> &'static str {
        match self {
            Self::Plain => "txt",
            Self::Markdown => "md",
            Self::Xml => "xml",
        }
    }
}

/// Pack options. All paths are processed locally; nothing is uploaded.
#[derive(Debug, Clone)]
pub struct Options {
    pub roots: Vec<PathBuf>,
    pub gitignore: bool,
    pub hidden: bool,
    pub follow_links: bool,
    pub max_file_size: u64,
    /// Threads that extract files. `0` means Rayon's default (usually
    /// available parallelism). The walk runs on one thread, so the order it
    /// finds files in, and what the budgets keep, never depends on this.
    pub jobs: usize,
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub follow_archives: bool,
    pub skip_binaries: bool,
    pub tree: TreeMode,
    pub format: OutputFormat,
    pub notebook_outputs: bool,
    pub quiet: bool,
    pub list_only: bool,
    pub tokens: bool,
    /// Which discovered files to emit. [`Selection::Only`] with an empty
    /// list matches nothing; it never becomes “all files”.
    pub selection: Selection,
    /// Skip these filesystem paths (canonical or as given), e.g. the output file.
    pub skip_paths: Vec<PathBuf>,
    /// Preserve HTML/XML/JSON as source instead of converting to readable text.
    pub source_mode: bool,
    /// Cap on discovered file entries. `0` means no cap.
    pub max_entries: usize,
    /// Cap on summed input sizes processed in one operation.
    pub max_total_bytes: u64,
}

/// Explicit pack/scan selection. An empty [`Only`] is not “everything”.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Selection {
    #[default]
    AllEligible,
    Only(Vec<String>),
}

impl Selection {
    #[must_use]
    pub fn allows(&self, relative: &str) -> bool {
        match self {
            Self::AllEligible => true,
            Self::Only(ids) => ids.iter().any(|id| id == relative),
        }
    }

    #[must_use]
    pub fn is_empty_only(&self) -> bool {
        matches!(self, Self::Only(ids) if ids.is_empty())
    }
}

impl Default for Options {
    fn default() -> Self {
        Self {
            roots: vec![PathBuf::from(".")],
            gitignore: true,
            hidden: false,
            follow_links: false,
            max_file_size: 8 * 1024 * 1024,
            jobs: 0,
            include: Vec::new(),
            exclude: default_exclude_globs(),
            follow_archives: false,
            skip_binaries: true,
            tree: TreeMode::Selected,
            format: OutputFormat::Plain,
            notebook_outputs: false,
            quiet: false,
            list_only: false,
            tokens: false,
            selection: Selection::AllEligible,
            skip_paths: Vec::new(),
            source_mode: false,
            max_entries: 0,
            max_total_bytes: 1 << 30,
        }
    }
}

/// Order `/`-separated paths the way a walk visits them: depth first, with
/// each directory's entries in name order. `a/b.txt` comes before `a-c.txt`,
/// since the directory `a` sorts before the name `a-c.txt`.
#[must_use]
pub fn cmp_path_order(a: &str, b: &str) -> Ordering {
    a.split('/').cmp(b.split('/'))
}

/// The entry and byte budgets of one pack, spent file by file in path order
/// ([`cmp_path_order`]).
///
/// Files are kept until the first that does not fit; that file and every
/// one after it are left out, so a folder keeps the same files in the walk,
/// in [`crate::pack_entries`], and in the browser mill. A file over
/// [`Options::max_file_size`] is never read, so it spends no bytes, but it
/// still counts as an entry.
#[derive(Debug, Clone)]
pub struct Budget {
    max_entries: usize,
    max_total_bytes: u64,
    max_file_size: u64,
    entries: usize,
    bytes: u64,
    cut: bool,
}

impl Budget {
    #[must_use]
    pub fn new(opts: &Options) -> Self {
        Self {
            max_entries: opts.max_entries,
            max_total_bytes: opts.max_total_bytes,
            max_file_size: opts.max_file_size,
            entries: 0,
            bytes: 0,
            cut: false,
        }
    }

    /// Spend the budgets on the next file, `size` bytes long. `false` when it
    /// does not fit: leave it out, and every file after it.
    pub fn take(&mut self, size: u64) -> bool {
        if self.cut {
            return false;
        }
        let cost = if size > self.max_file_size { 0 } else { size };
        let bytes = self.bytes.saturating_add(cost);
        let full = self.max_entries != 0 && self.entries >= self.max_entries;
        if full || bytes > self.max_total_bytes {
            self.cut = true;
            return false;
        }
        self.entries += 1;
        self.bytes = bytes;
        true
    }

    /// Whether a file has been left out.
    #[must_use]
    pub fn is_cut(&self) -> bool {
        self.cut
    }
}

/// Keep the longest prefix of `items`, already in path order, that fits the
/// budgets in `opts` (see [`Budget`]). Returns whether anything was cut.
pub fn apply_budgets<T>(items: &mut Vec<T>, opts: &Options, size: impl Fn(&T) -> u64) -> bool {
    let mut budget = Budget::new(opts);
    let kept = items
        .iter()
        .take_while(|item| budget.take(size(item)))
        .count();
    items.truncate(kept);
    budget.is_cut()
}

/// Parse a human size like `8MiB`, `1m`, or `500k` into bytes (1024-based).
pub fn parse_size(s: &str) -> Result<u64, String> {
    let lower = s.trim().replace('_', "").to_ascii_lowercase();
    let (num, mul) = if let Some(n) = lower.strip_suffix("kib") {
        (n, 1024_u64)
    } else if let Some(n) = lower.strip_suffix("mib") {
        (n, 1024 * 1024)
    } else if let Some(n) = lower.strip_suffix("gib") {
        (n, 1024 * 1024 * 1024)
    } else if let Some(n) = lower.strip_suffix("kb") {
        (n, 1024)
    } else if let Some(n) = lower.strip_suffix("mb") {
        (n, 1024 * 1024)
    } else if let Some(n) = lower.strip_suffix("gb") {
        (n, 1024 * 1024 * 1024)
    } else if let Some(n) = lower.strip_suffix('k') {
        (n, 1024)
    } else if let Some(n) = lower.strip_suffix('m') {
        (n, 1024 * 1024)
    } else if let Some(n) = lower.strip_suffix('g') {
        (n, 1024 * 1024 * 1024)
    } else {
        (lower.as_str(), 1)
    };
    let n: u64 = num
        .trim()
        .parse()
        .map_err(|_| format!("invalid size {s}"))?;
    Ok(n.saturating_mul(mul))
}

/// Directory names for build output, dependency installs, caches, and vendored
/// toolchains. Matched as a whole path component, so `src/build.rs` is kept.
pub fn generated_dir_names() -> &'static [&'static str] {
    &[
        ".angular",
        ".astro",
        ".bin",
        ".cache",
        ".dart_tool",
        ".docusaurus",
        ".eggs",
        ".figure-audit",
        ".git",
        ".gradle",
        ".hg",
        ".hypothesis",
        ".ipynb_checkpoints",
        ".mypy_cache",
        ".next",
        ".nox",
        ".nuxt",
        ".open-next",
        ".output",
        ".parcel-cache",
        ".pnp",
        ".pytest_cache",
        ".ruff_cache",
        ".serverless",
        ".svelte-kit",
        ".svn",
        ".tox",
        ".turbo",
        ".venv",
        ".vercel",
        ".vite",
        ".worktrees",
        ".wrangler",
        ".yarn",
        "CMakeFiles",
        "__pycache__",
        "bower_components",
        "build",
        "coverage",
        "dist",
        "htmlcov",
        "node_modules",
        "out",
        "runs",
        "site-packages",
        "storybook-static",
        "target",
        "toolchains",
        "vendor",
        "venv",
    ]
}

/// Globs applied on top of gitignore so noisy trees stay out even when
/// they are tracked or the folder is not a git repo.
pub fn default_exclude_globs() -> Vec<String> {
    let mut globs: Vec<String> = generated_dir_names()
        .iter()
        .map(|dir| format!("{dir}/**"))
        .collect();
    globs.extend(
        [
            "*.min.js",
            "*.min.css",
            "*.tsbuildinfo",
            "next-env.d.ts",
            "*.egg-info/**",
            "*.dist-info/**",
            "package-lock.json",
            "yarn.lock",
            "pnpm-lock.yaml",
            "bun.lock",
            "bun.lockb",
            ".env",
            ".env.*",
            "*.pem",
            "*.key",
            "id_rsa",
            "id_rsa.*",
            "*.pyc",
            "*.pyo",
            "*.class",
            "*.o",
            "*.a",
            "*.so",
            "*.dylib",
            "*.dll",
            "*.exe",
            "*.wasm",
            "*.bin",
            ".DS_Store",
            "Thumbs.db",
        ]
        .into_iter()
        .map(str::to_string),
    );
    globs
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn test_from_ext_with_txt_md_xml_returns_matching_format() {
        assert_eq!(OutputFormat::from_ext("txt"), Some(OutputFormat::Plain));
        assert_eq!(OutputFormat::from_ext(".md"), Some(OutputFormat::Markdown));
        assert_eq!(OutputFormat::from_ext("XML"), Some(OutputFormat::Xml));
    }

    #[test]
    fn test_from_path_with_dump_md_returns_markdown() {
        assert_eq!(
            OutputFormat::from_path(Path::new("out/dump.md")),
            Some(OutputFormat::Markdown)
        );
        assert_eq!(
            OutputFormat::from_path(Path::new("out/dump.txt")),
            Some(OutputFormat::Plain)
        );
        assert_eq!(
            OutputFormat::from_path(Path::new("out/dump.xml")),
            Some(OutputFormat::Xml)
        );
    }

    #[test]
    fn test_parse_size_with_mib_suffix_returns_bytes() {
        assert_eq!(parse_size("8MiB").unwrap(), 8 * 1024 * 1024);
        assert_eq!(parse_size("1m").unwrap(), 1024 * 1024);
        assert_eq!(parse_size("500k").unwrap(), 500 * 1024);
    }

    #[test]
    fn test_selection_only_empty_allows_nothing() {
        let sel = Selection::Only(Vec::new());
        assert!(sel.is_empty_only());
        assert!(!sel.allows("src/lib.rs"));
        assert!(Selection::AllEligible.allows("src/lib.rs"));
        assert!(Selection::Only(vec!["src/lib.rs".into()]).allows("src/lib.rs"));
        assert!(!Selection::Only(vec!["src/lib.rs".into()]).allows("src/main.rs"));
    }

    #[test]
    fn test_cmp_path_order_with_directory_and_names_returns_walk_order() {
        let mut paths = vec!["a.txt", "a/b.txt", "a-c.txt", "B.txt", "a/a/z.txt"];
        paths.sort_by(|a, b| cmp_path_order(a, b));
        assert_eq!(paths, ["B.txt", "a/a/z.txt", "a/b.txt", "a-c.txt", "a.txt"]);
    }

    #[test]
    fn test_apply_budgets_with_file_that_does_not_fit_stops_there() {
        let opts = Options {
            max_total_bytes: 100,
            max_file_size: 1000,
            ..Options::default()
        };
        let mut sizes = vec![60, 50, 10];
        assert!(apply_budgets(&mut sizes, &opts, |size| *size));
        assert_eq!(sizes, [60]);

        // Over the per-file cap: never read, so no bytes, but one entry.
        let mut sizes = vec![5000, 100, 1];
        assert!(apply_budgets(&mut sizes, &opts, |size| *size));
        assert_eq!(sizes, [5000, 100]);

        let entries = Options {
            max_entries: 2,
            ..Options::default()
        };
        let mut sizes = vec![1, 2];
        assert!(!apply_budgets(&mut sizes, &entries, |size| *size));
        let mut sizes = vec![1, 2, 3];
        assert!(apply_budgets(&mut sizes, &entries, |size| *size));
        assert_eq!(sizes, [1, 2]);
    }

    #[test]
    fn test_budget_with_refused_file_refuses_every_later_one() {
        let mut budget = Budget::new(&Options {
            max_total_bytes: 10,
            ..Options::default()
        });
        assert!(budget.take(10));
        assert!(!budget.is_cut());
        assert!(!budget.take(1));
        assert!(!budget.take(0));
        assert!(budget.is_cut());
    }

    #[test]
    fn test_default_exclude_globs_include_next_out_and_toolchains() {
        let globs = default_exclude_globs();
        for pat in [
            ".next/**",
            "out/**",
            "toolchains/**",
            "runs/**",
            ".worktrees/**",
            "node_modules/**",
            "*.bin",
        ] {
            assert!(globs.iter().any(|g| g == pat), "missing {pat}");
        }
    }
}
