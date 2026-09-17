//! Who we are compiling on: OS, arch, and known boxes.

use std::path::Path;
use std::thread;

/// OS of the rustc host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostOs {
    Macos,
    Linux,
    Windows,
    Other,
}

/// CPU architecture of the rustc host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arch {
    Aarch64,
    X86_64,
    Other,
}

/// Named machines we tune for. Hostname match is case-insensitive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Machine {
    /// MacBook Pro M1 Pro (this laptop). Hostname `Matrix`.
    Matrix,
    /// Ryzen 9 9950X3D + RTX 5090 Ubuntu box.
    Manifold,
    /// Former 3900X + 3080 box.
    Tensor,
    /// Raspberry Pi 5.
    Jacobian,
    /// Raspberry Pi 4.
    Hessian,
    Unknown,
}

/// Snapshot used to drive cargo env, jobs, and rustflags.
#[derive(Debug, Clone)]
pub struct Host {
    pub os: HostOs,
    pub arch: Arch,
    pub machine: Machine,
    pub hostname: String,
    pub jobs: usize,
    pub env: Vec<(String, String)>,
    pub rustflags: Vec<String>,
}

impl Host {
    #[must_use]
    pub fn detect() -> Self {
        let os = detect_os();
        let arch = detect_arch();
        let hostname = hostname();
        let machine = Machine::from_hostname(&hostname);
        let jobs = jobs_for(
            machine,
            thread::available_parallelism()
                .map(usize::from)
                .unwrap_or(4),
        );
        let env = env_for(os);
        Self {
            os,
            arch,
            machine,
            hostname,
            jobs,
            env,
            rustflags: Vec::new(),
        }
    }

    /// Extra rustc flags for a release build on a box we know.
    #[must_use]
    pub fn with_release_flags(mut self) -> Self {
        if matches!(self.os, HostOs::Macos | HostOs::Linux) && !matches!(self.arch, Arch::Other) {
            self.rustflags.push("-Ctarget-cpu=native".into());
        }
        self
    }

    #[must_use]
    pub fn label(&self) -> String {
        format!(
            "{} {}/{} ({} jobs)",
            self.machine.as_str(),
            self.os.as_str(),
            self.arch.as_str(),
            self.jobs
        )
    }
}

impl Machine {
    #[must_use]
    pub fn from_hostname(hostname: &str) -> Self {
        let stem = hostname
            .split('.')
            .next()
            .unwrap_or(hostname)
            .to_ascii_lowercase();
        match stem.as_str() {
            "matrix" => Self::Matrix,
            "manifold" => Self::Manifold,
            "tensor" => Self::Tensor,
            "jacobian" => Self::Jacobian,
            "hessian" => Self::Hessian,
            _ => Self::Unknown,
        }
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Matrix => "matrix",
            Self::Manifold => "manifold",
            Self::Tensor => "tensor",
            Self::Jacobian => "jacobian",
            Self::Hessian => "hessian",
            Self::Unknown => "unknown",
        }
    }
}

impl HostOs {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Macos => "macos",
            Self::Linux => "linux",
            Self::Windows => "windows",
            Self::Other => "other",
        }
    }
}

impl Arch {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Aarch64 => "aarch64",
            Self::X86_64 => "x86_64",
            Self::Other => "other",
        }
    }
}

fn detect_os() -> HostOs {
    match std::env::consts::OS {
        "macos" => HostOs::Macos,
        "linux" => HostOs::Linux,
        "windows" => HostOs::Windows,
        _ => HostOs::Other,
    }
}

fn detect_arch() -> Arch {
    match std::env::consts::ARCH {
        "aarch64" => Arch::Aarch64,
        "x86_64" => Arch::X86_64,
        _ => Arch::Other,
    }
}

fn hostname() -> String {
    std::env::var("PULP_HOST")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            hostname_cmd()
                .or_else(|| std::env::var("HOSTNAME").ok())
                .or_else(|| std::env::var("COMPUTERNAME").ok())
        })
        .unwrap_or_else(|| "unknown".into())
}

fn hostname_cmd() -> Option<String> {
    let out = std::process::Command::new("hostname").output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

fn jobs_for(machine: Machine, detected: usize) -> usize {
    let cap = match machine {
        Machine::Manifold => 24,
        Machine::Tensor => 16,
        Machine::Jacobian => 4,
        Machine::Hessian => 2,
        Machine::Matrix => 8,
        Machine::Unknown => detected,
    };
    cap.min(detected).max(1)
}

fn env_for(os: HostOs) -> Vec<(String, String)> {
    match os {
        HostOs::Macos => macos_clt_env(),
        _ => Vec::new(),
    }
}

const CLT: &str = "/Library/Developer/CommandLineTools";

fn macos_clt_env() -> Vec<(String, String)> {
    if !Path::new(CLT).is_dir() {
        return Vec::new();
    }
    let sdk = format!("{CLT}/SDKs/MacOSX.sdk");
    let clang = format!("{CLT}/usr/bin/clang");
    let clangxx = format!("{CLT}/usr/bin/clang++");
    if !Path::new(&clang).exists() {
        return Vec::new();
    }
    vec![
        ("DEVELOPER_DIR".into(), CLT.into()),
        ("SDKROOT".into(), sdk),
        ("CC".into(), clang.clone()),
        ("CXX".into(), clangxx),
        ("CC_aarch64-apple-darwin".into(), clang.clone()),
        ("CC_aarch64_apple_darwin".into(), clang.clone()),
        ("CC_x86_64-apple-darwin".into(), clang.clone()),
        ("CC_x86_64_apple_darwin".into(), clang),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_from_hostname_with_matrix_returns_matrix() {
        assert_eq!(Machine::from_hostname("Matrix"), Machine::Matrix);
        assert_eq!(Machine::from_hostname("matrix.local"), Machine::Matrix);
    }

    #[test]
    fn test_from_hostname_with_manifold_returns_manifold() {
        assert_eq!(Machine::from_hostname("manifold"), Machine::Manifold);
        assert_eq!(
            Machine::from_hostname("manifold.tailf7d439.ts.net"),
            Machine::Manifold
        );
    }

    #[test]
    fn test_from_hostname_with_pi_boxes_returns_named_machines() {
        assert_eq!(Machine::from_hostname("Jacobian"), Machine::Jacobian);
        assert_eq!(Machine::from_hostname("hessian"), Machine::Hessian);
    }

    #[test]
    fn test_from_hostname_with_unknown_returns_unknown() {
        assert_eq!(Machine::from_hostname("laptop-3"), Machine::Unknown);
    }

    #[test]
    fn test_jobs_for_with_manifold_caps_below_detected() {
        assert_eq!(jobs_for(Machine::Manifold, 32), 24);
        assert_eq!(jobs_for(Machine::Hessian, 8), 2);
        assert_eq!(jobs_for(Machine::Matrix, 8), 8);
        assert_eq!(jobs_for(Machine::Unknown, 12), 12);
    }
}
