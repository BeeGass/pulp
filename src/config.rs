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
    /// `0` means Rayon's default (usually available parallelism).
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
    /// Cap on discovered file entries (examined, not only emitted).
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
            max_entries: 100_000,
            max_total_bytes: 1 << 30,
        }
    }
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

/// Globs applied on top of gitignore so noisy trees stay out even when
/// they are tracked or the folder is not a git repo.
pub fn default_exclude_globs() -> Vec<String> {
    [
        "node_modules/**",
        "target/**",
        "dist/**",
        "build/**",
        ".venv/**",
        "venv/**",
        "__pycache__/**",
        ".mypy_cache/**",
        ".pytest_cache/**",
        ".ruff_cache/**",
        ".git/**",
        ".hg/**",
        ".svn/**",
        ".next/**",
        ".nuxt/**",
        "coverage/**",
        "vendor/**",
        "*.min.js",
        "*.min.css",
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
        ".DS_Store",
        "Thumbs.db",
    ]
    .into_iter()
    .map(str::to_string)
    .collect()
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
}
