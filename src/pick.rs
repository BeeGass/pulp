//! Native folder picker. Opens the OS file manager, not a browser widget.
//!
//! A localhost mill cannot see the real path from `<input type="file">`.
//! The mill process opens Finder / the portal / Explorer instead.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

/// Open a directory chooser and return the selected folder.
///
/// `Ok(None)` means the user cancelled. `Err` is a picker failure.
pub fn pick_folder() -> Result<Option<PathBuf>, String> {
    pick_platform()
}

#[cfg(target_os = "macos")]
fn pick_platform() -> Result<Option<PathBuf>, String> {
    pick_macos()
}

#[cfg(target_os = "linux")]
fn pick_platform() -> Result<Option<PathBuf>, String> {
    pick_linux()
}

#[cfg(target_os = "windows")]
fn pick_platform() -> Result<Option<PathBuf>, String> {
    pick_windows()
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn pick_platform() -> Result<Option<PathBuf>, String> {
    Err("folder picker is not supported on this OS".into())
}

#[cfg(target_os = "macos")]
fn pick_macos() -> Result<Option<PathBuf>, String> {
    // Finder's "choose folder" is the file-manager dialog. osascript works from
    // a CLI mill that has no GUI event loop (unlike NSOpenPanel on a worker).
    const SCRIPT: &str = r#"
tell application "Finder"
    activate
    set pulpFolder to choose folder with prompt "Choose a folder to pulp"
    return POSIX path of pulpFolder
end tell
"#;
    let mut child = Command::new("osascript")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| format!("osascript: {err}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(SCRIPT.as_bytes())
            .map_err(|err| format!("osascript stdin: {err}"))?;
    }
    let output = child
        .wait_with_output()
        .map_err(|err| format!("osascript: {err}"))?;
    parse_picker_output(
        output.status.success(),
        &String::from_utf8_lossy(&output.stdout),
        &String::from_utf8_lossy(&output.stderr),
    )
}

#[cfg(target_os = "linux")]
fn pick_linux() -> Result<Option<PathBuf>, String> {
    if let Some(out) = try_cmd(
        "zenity",
        &[
            "--file-selection",
            "--directory",
            "--title=Choose a folder to pulp",
        ],
    ) {
        return out;
    }
    if let Some(home) = std::env::var_os("HOME") {
        let home = home.to_string_lossy();
        if let Some(out) = try_cmd(
            "kdialog",
            &[
                "--getexistingdirectory",
                &home,
                "--title",
                "Choose a folder to pulp",
            ],
        ) {
            return out;
        }
    }
    Err("no folder picker found (install zenity or kdialog)".into())
}

#[cfg(target_os = "windows")]
fn pick_windows() -> Result<Option<PathBuf>, String> {
    const SCRIPT: &str = r#"
Add-Type -AssemblyName System.Windows.Forms
$dialog = New-Object System.Windows.Forms.FolderBrowserDialog
$dialog.Description = 'Choose a folder to pulp'
$dialog.ShowNewFolderButton = $false
if ($dialog.ShowDialog() -eq [System.Windows.Forms.DialogResult]::OK) {
    Write-Output $dialog.SelectedPath
}
"#;
    let output = Command::new("powershell")
        .args(["-NoProfile", "-STA", "-Command", SCRIPT])
        .output()
        .map_err(|err| format!("powershell: {err}"))?;
    parse_picker_output(
        output.status.success(),
        &String::from_utf8_lossy(&output.stdout),
        &String::from_utf8_lossy(&output.stderr),
    )
}

#[cfg(target_os = "linux")]
fn try_cmd(bin: &str, args: &[&str]) -> Option<Result<Option<PathBuf>, String>> {
    let output = Command::new(bin).args(args).output().ok()?;
    Some(parse_picker_output(
        output.status.success(),
        &String::from_utf8_lossy(&output.stdout),
        &String::from_utf8_lossy(&output.stderr),
    ))
}

pub(crate) fn parse_picker_output(
    ok: bool,
    stdout: &str,
    stderr: &str,
) -> Result<Option<PathBuf>, String> {
    let out = stdout.trim();
    let err = stderr.trim();
    if !ok {
        if out.is_empty() {
            return Ok(None);
        }
        return Err(if err.is_empty() {
            "folder picker failed".into()
        } else {
            err.to_string()
        });
    }
    if out.is_empty() {
        return Ok(None);
    }
    Ok(Some(PathBuf::from(out)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_picker_output_with_posix_path_returns_folder() {
        let path = parse_picker_output(true, "/Users/beegass/Projects/pulp/\n", "").unwrap();
        assert_eq!(
            path.unwrap(),
            PathBuf::from("/Users/beegass/Projects/pulp/")
        );
    }

    #[test]
    fn test_parse_picker_output_with_cancel_stderr_returns_none() {
        let path = parse_picker_output(false, "", "User canceled.").unwrap();
        assert!(path.is_none());
    }

    #[test]
    fn test_parse_picker_output_with_empty_success_returns_none() {
        let path = parse_picker_output(true, "  \n", "").unwrap();
        assert!(path.is_none());
    }
}
