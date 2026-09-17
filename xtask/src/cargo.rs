//! Spawn cargo with host-specific jobs and env.

use std::env;
use std::path::PathBuf;
use std::process::{Command, ExitStatus};

use anyhow::{Context, bail};

use crate::host::Host;

pub fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask lives in workspace/xtask")
        .to_path_buf()
}

pub fn run(host: &Host, args: &[String]) -> anyhow::Result<()> {
    let status = status(host, args)?;
    if !status.success() {
        bail!("cargo {} failed", args.join(" "));
    }
    Ok(())
}

pub fn status(host: &Host, args: &[String]) -> anyhow::Result<ExitStatus> {
    let mut cmd = Command::new(env::var("CARGO").unwrap_or_else(|_| "cargo".into()));
    cmd.current_dir(workspace_root());
    cmd.args(args);
    cmd.env("CARGO_BUILD_JOBS", host.jobs.to_string());
    for (key, value) in &host.env {
        cmd.env(key, value);
    }
    if !host.rustflags.is_empty() {
        let extra = host.rustflags.join(" ");
        let merged = match env::var("RUSTFLAGS") {
            Ok(existing) if !existing.is_empty() => format!("{existing} {extra}"),
            _ => extra,
        };
        cmd.env("RUSTFLAGS", merged);
    }
    cmd.status()
        .with_context(|| format!("spawn cargo {}", args.join(" ")))
}
