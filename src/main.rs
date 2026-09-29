use std::io::{self, BufWriter, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::Context;
use clap::{Parser, Subcommand, ValueEnum};

use pulp::config::parse_size;
use pulp::tree::display_path;
use pulp::{Options, OutputFormat, TreeMode};

/// Pulp a local folder of mixed documents into one LLM-ready text file.
#[derive(Parser, Debug)]
#[command(name = "pulp", version, about, args_conflicts_with_subcommands = true)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// Files, directories, or archives. Defaults to the current directory.
    #[arg(default_value = ".")]
    paths: Vec<PathBuf>,

    /// Write the dump here instead of stdout (`.txt`, `.md`, `.xml` select format).
    #[arg(short, long)]
    output: Option<PathBuf>,

    /// Output layout: txt/plain, md/markdown, xml. Inferred from -o when omitted.
    #[arg(short, long, value_name = "FMT")]
    format: Option<String>,

    /// Directory map: selected, full, or none.
    #[arg(long, value_enum, default_value_t = TreeCli::Selected)]
    tree: TreeCli,

    /// Omit the directory map.
    #[arg(long)]
    no_tree: bool,

    /// Worker threads (0 = auto).
    #[arg(short, long, default_value_t = 0)]
    jobs: usize,

    /// Skip files larger than this (e.g. 8MiB, 1m, 500k).
    #[arg(long, default_value = "8MiB", value_parser = parse_size)]
    max_file_size: u64,

    /// Stop after this many discovered files. `0` (the default) means no cap.
    #[arg(long, default_value_t = 0)]
    max_entries: usize,

    /// Stop after this much summed input (e.g. 1GiB).
    #[arg(long, default_value = "1GiB", value_parser = parse_size)]
    max_total_bytes: u64,

    /// Include glob (repeatable). `*.rs` also matches nested paths.
    #[arg(long)]
    include: Vec<String>,

    /// Extra exclude glob (repeatable).
    #[arg(long)]
    exclude: Vec<String>,

    /// Do not apply the built-in exclude list.
    #[arg(long)]
    no_default_excludes: bool,

    /// Include hidden files (except `.git`).
    #[arg(long)]
    hidden: bool,

    /// Ignore `.gitignore` / `.ignore`.
    #[arg(long)]
    no_gitignore: bool,

    /// Follow symlinks.
    #[arg(long)]
    follow_links: bool,

    /// Recurse into nested zip/tar members.
    #[arg(long)]
    archives: bool,

    /// Include binary placeholders instead of skipping binaries.
    #[arg(long)]
    binaries: bool,

    /// Include Jupyter cell outputs.
    #[arg(long)]
    notebook_outputs: bool,

    /// Keep HTML, XML, and JSON as source instead of converting to readable text.
    #[arg(long)]
    source: bool,

    /// Print the token estimate (also part of the summary).
    #[arg(long)]
    tokens: bool,

    /// List paths that would be pulped; do not extract.
    #[arg(long)]
    list: bool,

    /// Suppress the stderr summary. Warnings about paths that could not be
    /// walked still print.
    #[arg(short, long)]
    quiet: bool,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Open the local mill in a browser (127.0.0.1 only).
    Ui {
        /// Port on localhost. Default 8747; if that is busy, the next free port is used.
        #[arg(short, long)]
        port: Option<u16>,
        /// Do not open a browser.
        #[arg(long)]
        no_open: bool,
    },
    /// Internal: extract one heavy file in a child process.
    #[command(name = "__extract", hide = true)]
    Extract {
        /// Read the file here.
        #[arg(long, required_unless_present = "stdin", conflicts_with = "stdin")]
        path: Option<PathBuf>,
        /// Read the file's bytes from stdin.
        #[arg(long)]
        stdin: bool,
        #[arg(long)]
        kind: String,
        #[arg(long, default_value_t = 8 * 1024 * 1024)]
        max_file_size: u64,
        #[arg(long)]
        source: bool,
        #[arg(long)]
        notebook_outputs: bool,
    },
}

#[derive(Clone, Copy, Debug, Default, ValueEnum)]
enum TreeCli {
    #[default]
    Selected,
    Full,
    None,
}

fn main() -> anyhow::Result<()> {
    // Heavy parsers run in a child `pulp` whatever this executable is named.
    pulp::extract::isolate::set_isolation(true);
    let cli = Cli::parse();
    match cli.command {
        Some(Command::Ui { port, no_open }) => {
            let rt = tokio::runtime::Runtime::new()?;
            let preferred = port.unwrap_or(8747);
            let try_next = port.is_none();
            let served = rt.block_on(pulp::ui::serve(preferred, try_next, !no_open));
            // A pack still finishing its current file, or an open folder
            // picker, holds a blocking thread; the mill has stopped, so the
            // process does not wait on it.
            rt.shutdown_timeout(std::time::Duration::from_secs(1));
            return served;
        }
        Some(Command::Extract {
            path,
            stdin,
            kind,
            max_file_size,
            source,
            notebook_outputs,
        }) => {
            pulp::extract::isolate::start_child_watchdog();
            pulp::extract::isolate::set_child_panic_hook();
            let kind = pulp::kind_from_label(&kind)
                .ok_or_else(|| anyhow::anyhow!("unknown kind {kind}"))?;
            let opts = pulp::extract::ExtractOpts {
                max_file_size,
                notebook_outputs,
                source_mode: source,
            };
            let result = match path {
                Some(path) if !stdin => pulp::extract::isolate::run_child(&path, kind, &opts),
                _ => pulp::extract::isolate::run_child_reader(
                    io::stdin().lock(),
                    "file",
                    kind,
                    &opts,
                ),
            };
            match result {
                Ok(()) => return Ok(()),
                Err(code) => std::process::exit(code),
            }
        }
        None => {}
    }
    let format = resolve_format(&cli)?;
    let tree = if cli.no_tree {
        TreeMode::None
    } else {
        match cli.tree {
            TreeCli::Selected => TreeMode::Selected,
            TreeCli::Full => TreeMode::Full,
            TreeCli::None => TreeMode::None,
        }
    };
    let mut skip_paths = Vec::new();
    if let Some(out) = &cli.output {
        skip_paths.push(out.clone());
        if let Ok(cwd) = std::env::current_dir() {
            skip_paths.push(cwd.join(out));
        }
        skip_paths = pulp::walk::normalize_skip_paths(&skip_paths);
    }
    let opts = Options {
        roots: cli.paths,
        gitignore: !cli.no_gitignore,
        hidden: cli.hidden,
        follow_links: cli.follow_links,
        max_file_size: cli.max_file_size,
        jobs: cli.jobs,
        include: cli.include,
        exclude: cli.exclude,
        default_excludes: !cli.no_default_excludes,
        follow_archives: cli.archives,
        skip_binaries: !cli.binaries,
        tree,
        format,
        notebook_outputs: cli.notebook_outputs,
        quiet: cli.quiet,
        list_only: cli.list,
        tokens: cli.tokens,
        selection: pulp::Selection::AllEligible,
        skip_paths,
        source_mode: cli.source,
        max_entries: cli.max_entries,
        max_total_bytes: cli.max_total_bytes,
    };
    let start = Instant::now();
    let (manifest, warnings) =
        pulp::manifest::scan_manifest_with_warnings(&opts).context("pulp failed")?;
    let packed =
        pulp::pack_manifest(&manifest, &opts, None, None, Some(start)).context("pulp failed")?;
    if opts.list_only {
        for file in &packed.files {
            println!("{}", display_path(&file.relative));
        }
    } else if let Some(path) = &cli.output {
        let file =
            std::fs::File::create(path).with_context(|| format!("create {}", path.display()))?;
        let mut w = BufWriter::with_capacity(64 * 1024, file);
        pulp::render::write_all(&mut w, &packed, &opts)?;
        w.flush()?;
    } else {
        let stdout = io::stdout();
        let mut w = BufWriter::with_capacity(64 * 1024, stdout.lock());
        pulp::render::write_all(&mut w, &packed, &opts)?;
        w.flush()?;
    }
    // Walk warnings print even with --quiet, which hides only the summary.
    for line in warning_lines(&warnings) {
        eprintln!("{line}");
    }
    if !opts.quiet {
        print_summary(&packed, opts.tokens);
    }
    Ok(())
}

fn resolve_format(cli: &Cli) -> anyhow::Result<OutputFormat> {
    if let Some(fmt) = &cli.format {
        OutputFormat::from_ext(fmt)
            .ok_or_else(|| anyhow::anyhow!("unknown format {fmt:?}; expected txt, md, or xml"))
    } else if let Some(path) = &cli.output {
        Ok(OutputFormat::from_path(path).unwrap_or(OutputFormat::Plain))
    } else {
        Ok(OutputFormat::Plain)
    }
}

/// Warnings printed before the summary: the first few paths the walk could
/// not read, then a count of the rest.
fn warning_lines(warnings: &pulp::walk::WalkWarnings) -> Vec<String> {
    const SHOWN: usize = 10;
    let mut lines: Vec<String> = warnings
        .messages
        .iter()
        .take(SHOWN)
        .map(|message| format!("warning: {}", display_path(message)))
        .collect();
    let rest = warnings.total.saturating_sub(lines.len());
    if rest > 0 {
        lines.push(format!("warning: {rest} more paths could not be walked"));
    }
    lines
}

fn print_summary(packed: &pulp::Packed, show_tokens: bool) {
    let _ = show_tokens;
    eprintln!("{}", summary_line(packed));
}

/// The stderr summary. Unreadable files are counted apart from the skipped
/// ones: pulp tried them, and the dump holds a note for each.
fn summary_line(packed: &pulp::Packed) -> String {
    let stats = &packed.stats;
    let unreadable = packed
        .files
        .iter()
        .filter(|file| matches!(file.status, pulp::FileStatus::Unreadable(_)))
        .count();
    let skipped = stats.files_skipped.saturating_sub(unreadable);
    let ms = duration_ms(stats.elapsed);
    let mut line = format!(
        "pulped {} files ({} read, {} chars, ~{} tokens) in {ms}ms",
        stats.files_extracted,
        human_bytes(stats.bytes_read),
        stats.chars_emitted,
        stats.tokens_est
    );
    if unreadable > 0 {
        line.push_str(&format!(", {unreadable} unreadable"));
    }
    if skipped > 0 {
        line.push_str(&format!(", {skipped} skipped"));
    }
    if stats.truncated {
        line.push_str(", truncated");
    }
    if stats.cancelled {
        line.push_str(", cancelled");
    }
    line
}

fn duration_ms(d: Duration) -> u128 {
    d.as_millis()
}

fn human_bytes(n: u64) -> String {
    const K: f64 = 1024.0;
    let f = n as f64;
    if f < K {
        format!("{n} B")
    } else if f < K * K {
        format!("{:.1} KiB", f / K)
    } else if f < K * K * K {
        format!("{:.1} MiB", f / (K * K))
    } else {
        format!("{:.1} GiB", f / (K * K * K))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_size_with_mib_suffix_returns_bytes() {
        assert_eq!(parse_size("8MiB").unwrap(), 8 * 1024 * 1024);
        assert_eq!(parse_size("1m").unwrap(), 1024 * 1024);
        assert_eq!(parse_size("500k").unwrap(), 500 * 1024);
    }

    #[test]
    fn test_resolve_format_with_md_path_returns_markdown() {
        let cli = Cli::parse_from(["pulp", "-o", "dump.md"]);
        assert_eq!(resolve_format(&cli).unwrap(), OutputFormat::Markdown);
    }

    #[test]
    fn test_resolve_format_with_xml_flag_returns_xml() {
        let cli = Cli::parse_from(["pulp", "-f", "xml"]);
        assert_eq!(resolve_format(&cli).unwrap(), OutputFormat::Xml);
    }

    #[test]
    fn test_summary_line_with_unreadable_file_returns_it_apart_from_skipped() {
        let file = |name: &str, status: pulp::FileStatus| pulp::PackedFile {
            id: name.into(),
            relative: name.into(),
            kind: pulp::Kind::Text,
            size: 4,
            text: String::new(),
            status,
        };
        let mut packed = pulp::Packed {
            files: vec![
                file("a.rs", pulp::FileStatus::Extracted),
                file("paper.pdf", pulp::FileStatus::Unreadable("bad".into())),
                file("logo.png", pulp::FileStatus::SkippedBinary),
            ],
            tree: String::new(),
            stats: pulp::Stats {
                files_extracted: 1,
                files_skipped: 2,
                ..pulp::Stats::default()
            },
        };
        let line = summary_line(&packed);
        assert!(line.starts_with("pulped 1 files ("), "{line}");
        assert!(line.ends_with(" in 0ms, 1 unreadable, 1 skipped"), "{line}");

        packed.files.remove(1);
        packed.stats.files_skipped = 1;
        let line = summary_line(&packed);
        assert!(line.ends_with(" in 0ms, 1 skipped"), "{line}");
    }

    #[test]
    fn test_warning_lines_with_many_warnings_returns_first_ten_and_a_count() {
        let warnings = pulp::walk::WalkWarnings {
            messages: (0..12)
                .map(|i| format!("/p{i:02}: Permission denied"))
                .collect(),
            total: 15,
        };
        let lines = warning_lines(&warnings);
        assert_eq!(lines.len(), 11);
        assert_eq!(lines[0], "warning: /p00: Permission denied");
        assert_eq!(lines[10], "warning: 5 more paths could not be walked");
        assert!(warning_lines(&pulp::walk::WalkWarnings::default()).is_empty());
    }

    #[test]
    fn test_cli_with_extract_stdin_flag_returns_no_path() {
        let cli = Cli::parse_from(["pulp", "__extract", "--stdin", "--kind", "pdf"]);
        match cli.command {
            Some(Command::Extract { path, stdin, .. }) => {
                assert!(stdin);
                assert!(path.is_none());
            }
            other => panic!("expected __extract, got {other:?}"),
        }
        assert!(Cli::try_parse_from(["pulp", "__extract", "--kind", "pdf"]).is_err());
    }

    #[test]
    fn test_cli_with_ui_subcommand_returns_ui_command() {
        let cli = Cli::parse_from(["pulp", "ui", "--port", "9000", "--no-open"]);
        match cli.command {
            Some(Command::Ui { port, no_open }) => {
                assert_eq!(port, Some(9000));
                assert!(no_open);
            }
            other => panic!("expected ui, got {other:?}"),
        }
    }
}
