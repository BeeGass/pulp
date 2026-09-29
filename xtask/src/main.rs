//! `cargo xtask` — per-OS / per-machine pulp builds.

use anyhow::Context;
use clap::{Parser, Subcommand};

mod cargo;
mod host;
mod site;
mod uitest;

use host::Host;

#[derive(Parser, Debug)]
#[command(name = "xtask", about = "Build pulp for this machine and OS.")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Print detected host, jobs, and env.
    Doctor,
    /// `cargo build` with this host's jobs and compiler env.
    Build {
        #[arg(long)]
        release: bool,
    },
    /// `cargo test --workspace`.
    Test,
    /// `cargo clippy --workspace --all-targets -- -D warnings`.
    Clippy,
    /// `cargo fmt --all`.
    Fmt {
        #[arg(long)]
        check: bool,
    },
    /// Build and run the local mill (`pulp ui`).
    Ui {
        #[arg(long)]
        port: Option<u16>,
        #[arg(long)]
        no_open: bool,
    },
    /// Copy the shared mill UI (web/) into the static site (site/).
    Site {
        /// Fail instead of copying when site/ is out of date.
        #[arg(long)]
        check: bool,
    },
    /// Run the mill's browser tests (web/test/) in headless Chrome or Chromium.
    UiTest {
        /// Serve web/ and site/ for a browser of your own instead of running Chrome.
        #[arg(long)]
        serve: bool,
    },
    /// `cargo run --` with trailing args (`cargo xtask run -- ui`).
    Run {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let host = Host::detect();
    match cli.command {
        Command::Doctor => doctor(&host),
        Command::Build { release } => build(&host, release),
        Command::Test => test(&host),
        Command::Clippy => clippy(&host),
        Command::Fmt { check } => fmt(&host, check),
        Command::Ui { port, no_open } => ui(&host, port, no_open),
        Command::Site { check } => sync_site(check),
        Command::UiTest { serve } => uitest::run(&cargo::workspace_root(), serve),
        Command::Run { args } => run_bin(&host, &args),
    }
}

fn doctor(host: &Host) -> anyhow::Result<()> {
    println!("pulp xtask host");
    println!("  name     {}", host.machine.as_str());
    println!("  hostname {}", host.hostname);
    println!("  os       {}", host.os.as_str());
    println!("  arch     {}", host.arch.as_str());
    println!("  jobs     {}", host.jobs);
    if host.env.is_empty() {
        println!("  env      (none extra)");
    } else {
        println!("  env");
        for (k, v) in &host.env {
            println!("           {k}={v}");
        }
    }
    Ok(())
}

fn build(host: &Host, release: bool) -> anyhow::Result<()> {
    let host = if release {
        host.clone().with_release_flags()
    } else {
        host.clone()
    };
    let mut args = vec!["build".into(), "--package".into(), "pulp".into()];
    if release {
        args.push("--release".into());
    }
    eprintln!("xtask build on {}", host.label());
    cargo::run(&host, &args)
}

fn test(host: &Host) -> anyhow::Result<()> {
    eprintln!("xtask test on {}", host.label());
    cargo::run(host, &["test".into(), "--workspace".into()])
}

fn clippy(host: &Host) -> anyhow::Result<()> {
    eprintln!("xtask clippy on {}", host.label());
    cargo::run(
        host,
        &[
            "clippy".into(),
            "--workspace".into(),
            "--all-targets".into(),
            "--".into(),
            "-D".into(),
            "warnings".into(),
        ],
    )
}

fn fmt(host: &Host, check: bool) -> anyhow::Result<()> {
    let mut args = vec!["fmt".into(), "--all".into()];
    if check {
        args.push("--".into());
        args.push("--check".into());
    }
    cargo::run(host, &args)
}

fn ui(host: &Host, port: Option<u16>, no_open: bool) -> anyhow::Result<()> {
    let mut args = vec![
        "run".into(),
        "--package".into(),
        "pulp".into(),
        "--".into(),
        "ui".into(),
    ];
    if let Some(port) = port {
        args.push("--port".into());
        args.push(port.to_string());
    }
    if no_open {
        args.push("--no-open".into());
    }
    eprintln!("xtask ui on {}", host.label());
    cargo::run(host, &args)
}

fn sync_site(check: bool) -> anyhow::Result<()> {
    if check {
        let stale = site::stale(&cargo::workspace_root());
        if stale.is_empty() {
            eprintln!("xtask site: site/ matches web/");
            return Ok(());
        }
        anyhow::bail!("site/ is out of date for {stale:?}; run `cargo xtask site`");
    }
    let changed = site::sync(&cargo::workspace_root())?;
    if changed.is_empty() {
        eprintln!("xtask site: site/ already matches web/");
    } else {
        for path in &changed {
            eprintln!("xtask site: updated {path}");
        }
    }
    Ok(())
}

fn run_bin(host: &Host, rest: &[String]) -> anyhow::Result<()> {
    let mut args = vec!["run".into(), "--package".into(), "pulp".into(), "--".into()];
    args.extend(rest.iter().cloned());
    cargo::run(host, &args).context("pulp run")
}
