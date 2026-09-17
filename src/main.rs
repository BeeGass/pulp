use std::io::{self, BufWriter, Write};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;
use clap::{Parser, Subcommand, ValueEnum};

use pulp::config::{default_exclude_globs, parse_size};
use pulp::{Options, OutputFormat, TreeMode, pack};

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

    /// Stop after this many discovered files.
    #[arg(long, default_value_t = 100_000)]
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

    /// Suppress the stderr summary.
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
}

#[derive(Clone, Copy, Debug, Default, ValueEnum)]
enum TreeCli {
    #[default]
    Selected,
    Full,
    None,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    if let Some(Command::Ui { port, no_open }) = cli.command {
        let rt = tokio::runtime::Runtime::new()?;
        let preferred = port.unwrap_or(8747);
        let try_next = port.is_none();
        return rt.block_on(pulp::ui::serve(preferred, try_next, !no_open));
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
    let mut exclude = if cli.no_default_excludes {
        Vec::new()
    } else {
        default_exclude_globs()
    };
    exclude.extend(cli.exclude);
    let mut skip_paths = Vec::new();
    if let Some(out) = &cli.output {
        skip_paths.push(out.clone());
        if let Ok(cwd) = std::env::current_dir() {
            skip_paths.push(cwd.join(out));
        }
        if let Ok(canon) = out.canonicalize() {
            skip_paths.push(canon);
        }
    }
    let opts = Options {
        roots: cli.paths,
        gitignore: !cli.no_gitignore,
        hidden: cli.hidden,
        follow_links: cli.follow_links,
        max_file_size: cli.max_file_size,
        jobs: cli.jobs,
        include: cli.include,
        exclude,
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
    let packed = pack(&opts).context("pulp failed")?;
    if opts.list_only {
        for file in &packed.files {
            println!("{}", file.relative);
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
    if !opts.quiet {
        print_summary(&packed.stats, opts.tokens);
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

fn print_summary(stats: &pulp::Stats, show_tokens: bool) {
    let _ = show_tokens;
    let ms = duration_ms(stats.elapsed);
    eprint!(
        "pulped {} files ({} text, ~{} tokens) in {ms}ms",
        stats.files_extracted,
        human_bytes(stats.chars_emitted as u64),
        stats.tokens_est
    );
    if stats.files_skipped > 0 {
        eprint!(", {} skipped", stats.files_skipped);
    }
    if stats.truncated {
        eprint!(", truncated");
    }
    eprintln!();
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
