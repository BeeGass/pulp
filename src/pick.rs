//! Native folder picker. Opens the OS file manager, not a browser widget.
//!
//! A localhost mill cannot see the real path from `<input type="file">`.
//! The mill process opens Finder / the portal / Explorer instead.

use std::path::PathBuf;
#[cfg(target_os = "macos")]
use std::process::{Command, Stdio};

/// Open a directory chooser and return the selected folder.
///
/// `Ok(None)` means the user cancelled. `Err` is a picker failure.
pub fn pick_folder() -> Result<Option<PathBuf>, String> {
    pick_platform()
}

/// What a picker process left behind.
#[derive(Debug, Clone, Default)]
pub(crate) struct Ran {
    /// Exit status code; `None` when a signal ended the process.
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl Ran {
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    fn from_output(output: std::process::Output) -> Self {
        Self {
            code: output.status.code(),
            stdout: output.stdout,
            stderr: output.stderr,
        }
    }
}

/// Whether a failed picker run means the user closed the dialog without
/// choosing. Gets the run and the last line of its stderr.
pub(crate) type CancelTest = fn(&Ran, &str) -> bool;

/// osascript fails with "User canceled. (-128)" on stderr.
#[cfg(any(target_os = "macos", test))]
fn said_user_canceled(_: &Ran, err: &str) -> bool {
    let e = err.to_ascii_lowercase();
    e.contains("user canceled") || e.contains("user cancelled") || e.contains("(-128)")
}

/// zenity and kdialog exit with status 1 and print nothing on stdout. GTK and
/// Qt often write warnings to stderr on the way out, so stderr alone says
/// nothing about whether the user cancelled. GTK also exits with status 1 when
/// it cannot reach the display (a stale `DISPLAY` over SSH, say); that is a
/// failure, not a cancel.
#[cfg(any(target_os = "linux", test))]
fn exited_one_silently(ran: &Ran, _: &str) -> bool {
    let stderr = String::from_utf8_lossy(&ran.stderr).to_ascii_lowercase();
    let no_display = [
        "cannot open display",
        "failed to open display",
        "unable to open display",
        "could not connect to display",
    ]
    .iter()
    .any(|needle| stderr.contains(needle));
    ran.code == Some(1) && ran.stdout.iter().all(u8::is_ascii_whitespace) && !no_display
}

#[cfg(target_os = "macos")]
fn pick_platform() -> Result<Option<PathBuf>, String> {
    pick_macos()
}

#[cfg(target_os = "linux")]
fn pick_platform() -> Result<Option<PathBuf>, String> {
    pick_linux_with(|name| std::env::var_os(name), run_program)
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
    use std::io::Write;
    // Finder's "choose folder" is the file-manager dialog. osascript works from
    // a CLI mill that has no GUI event loop (unlike NSOpenPanel on a worker).
    // The script is a constant and goes in on stdin; nothing from a request is
    // ever part of it.
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
        if let Err(err) = stdin.write_all(SCRIPT.as_bytes()) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("osascript stdin: {err}"));
        }
    }
    let output = child
        .wait_with_output()
        .map_err(|err| format!("osascript: {err}"))?;
    read_choice(
        &Ran {
            code: output.status.code(),
            stdout: output.stdout,
            stderr: output.stderr,
        },
        said_user_canceled,
    )
}

/// Run a picker program. `None` when it could not be started (not installed).
#[cfg(target_os = "linux")]
fn run_program(bin: &str, args: &[std::ffi::OsString]) -> Option<Ran> {
    std::process::Command::new(bin)
        .args(args)
        .output()
        .ok()
        .map(Ran::from_output)
}

/// The Linux picker: zenity, then kdialog. `var` reads the environment and
/// `run` starts a program, so tests can stand in for both.
#[cfg(any(target_os = "linux", test))]
fn pick_linux_with(
    var: impl Fn(&str) -> Option<std::ffi::OsString>,
    run: impl Fn(&str, &[std::ffi::OsString]) -> Option<Ran>,
) -> Result<Option<PathBuf>, String> {
    use std::ffi::OsString;
    // Without a display, zenity exits with status 1, the same status as a
    // cancel, and the mill would report "nothing chosen" for a dialog that
    // never appeared.
    let has_display = ["DISPLAY", "WAYLAND_DISPLAY"]
        .iter()
        .any(|name| var(name).is_some_and(|value| !value.is_empty()));
    if !has_display {
        return Err(
            "No desktop session to show a folder picker in (DISPLAY and WAYLAND_DISPLAY are unset). Enter a path manually."
                .into(),
        );
    }
    let zenity: Vec<OsString> = [
        "--file-selection",
        "--directory",
        "--title=Choose a folder to pulp",
    ]
    .iter()
    .map(OsString::from)
    .collect();
    if let Some(ran) = run("zenity", &zenity) {
        return read_choice(&ran, exited_one_silently);
    }
    let mut kdialog = vec![OsString::from("--getexistingdirectory")];
    // The start folder is a separate argument, never shell text.
    if let Some(home) = var("HOME").filter(|home| !home.is_empty()) {
        kdialog.push(home);
    }
    kdialog.push("--title".into());
    kdialog.push("Choose a folder to pulp".into());
    if let Some(ran) = run("kdialog", &kdialog) {
        return read_choice(&ran, exited_one_silently);
    }
    Err("no folder picker found (install zenity or kdialog)".into())
}

/// The folder dialog, as a constant PowerShell script. PowerShell writes to a
/// pipe in the console code page unless told otherwise, which mangles any
/// folder name outside that code page, so the script asks for UTF-8.
#[cfg(any(target_os = "windows", test))]
const WINDOWS_SCRIPT: &str = r#"
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8
Add-Type -AssemblyName System.Windows.Forms
$dialog = New-Object System.Windows.Forms.FolderBrowserDialog
$dialog.Description = 'Choose a folder to pulp'
$dialog.ShowNewFolderButton = $false
if ($dialog.ShowDialog() -eq [System.Windows.Forms.DialogResult]::OK) {
    Write-Output $dialog.SelectedPath
}
"#;

#[cfg(target_os = "windows")]
fn pick_windows() -> Result<Option<PathBuf>, String> {
    let output = std::process::Command::new("powershell")
        .args(["-NoProfile", "-STA", "-Command", WINDOWS_SCRIPT])
        .output()
        .map_err(|err| format!("powershell: {err}"))?;
    // A cancelled dialog prints nothing and exits 0; every failed run is an
    // error.
    read_choice(&Ran::from_output(output), |_, _| false)
}

/// The folder a picker run chose, `Ok(None)` when the user cancelled, or an
/// error to show in the mill.
pub(crate) fn read_choice(ran: &Ran, cancelled: CancelTest) -> Result<Option<PathBuf>, String> {
    let stderr = String::from_utf8_lossy(&ran.stderr);
    let err = last_line(&stderr);
    if ran.code == Some(0) {
        let Ok(stdout) = std::str::from_utf8(&ran.stdout) else {
            return Err(
                "The chosen folder's path is not valid UTF-8, which the mill cannot send to the page. Use the CLI for this folder: pulp -o dump.xml <folder>".into(),
            );
        };
        // Only the line ending goes: a folder name may begin or end with spaces.
        let chosen = stdout
            .strip_suffix('\n')
            .map(|line| line.strip_suffix('\r').unwrap_or(line))
            .unwrap_or(stdout);
        if chosen.trim().is_empty() {
            return Ok(None);
        }
        return Ok(Some(PathBuf::from(chosen)));
    }
    if cancelled(ran, err) {
        return Ok(None);
    }
    Err(if err.is_empty() {
        "Folder picker could not open. Enter a path manually.".into()
    } else {
        format!("{err}. Enter a path manually.")
    })
}

/// The last non-empty line of a picker's stderr: the error, after any
/// toolkit warnings printed before it.
fn last_line(stderr: &str) -> &str {
    stderr
        .lines()
        .map(str::trim)
        .rfind(|line| !line.is_empty())
        .unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::ffi::OsString;

    fn ran(code: i32, stdout: &str, stderr: &str) -> Ran {
        Ran {
            code: Some(code),
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    #[test]
    fn test_read_choice_with_posix_path_returns_folder() {
        let path = read_choice(
            &ran(0, "/Users/beegass/Projects/pulp/\n", ""),
            said_user_canceled,
        )
        .unwrap();
        assert_eq!(
            path.unwrap(),
            PathBuf::from("/Users/beegass/Projects/pulp/")
        );
    }

    #[test]
    fn test_read_choice_with_cancel_stderr_returns_none() {
        let path = read_choice(
            &ran(1, "", "execution error: User canceled. (-128)"),
            said_user_canceled,
        )
        .unwrap();
        assert!(path.is_none());
    }

    #[test]
    fn test_read_choice_with_empty_success_returns_none() {
        let path = read_choice(&ran(0, "  \n", ""), said_user_canceled).unwrap();
        assert!(path.is_none());
    }

    #[test]
    fn test_read_choice_with_fail_stderr_returns_error() {
        let err = read_choice(
            &ran(1, "", "execution error: no display"),
            said_user_canceled,
        )
        .unwrap_err();
        assert!(err.contains("Enter a path manually"), "{err}");
        assert!(err.contains("execution error"), "{err}");
    }

    #[test]
    fn test_read_choice_with_empty_fail_returns_error() {
        let err = read_choice(&ran(1, "", ""), said_user_canceled).unwrap_err();
        assert!(err.contains("could not open"), "{err}");
    }

    #[test]
    fn test_read_choice_with_edge_spaces_in_name_returns_them() {
        let path = read_choice(&ran(0, " /home/u/ notes \r\n", ""), exited_one_silently).unwrap();
        assert_eq!(path.unwrap(), PathBuf::from(" /home/u/ notes "));
    }

    #[test]
    fn test_read_choice_with_non_utf8_path_returns_error() {
        let chosen = Ran {
            code: Some(0),
            stdout: b"/home/u/caf\xe9\n".to_vec(),
            stderr: Vec::new(),
        };
        let err = read_choice(&chosen, exited_one_silently).unwrap_err();
        assert!(err.contains("not valid UTF-8"), "{err}");
    }

    #[test]
    fn test_read_choice_with_exit_one_and_toolkit_noise_returns_none() {
        let noise = "Gtk-Message: 10:12:01.123: GtkDialog mapped without a transient parent. This is discouraged.\n";
        let path = read_choice(&ran(1, "", noise), exited_one_silently).unwrap();
        assert!(path.is_none());
    }

    #[test]
    fn test_read_choice_with_exit_one_and_unreachable_display_returns_error() {
        let stderr =
            "\n(zenity:4242): Gtk-WARNING **: 10:12:01.123: cannot open display: localhost:10.0\n";
        let err = read_choice(&ran(1, "", stderr), exited_one_silently).unwrap_err();
        assert!(err.contains("cannot open display"), "{err}");
    }

    #[test]
    fn test_read_choice_with_exit_error_code_returns_last_stderr_line() {
        let stderr = "Gtk-WARNING: theme parse noise\nzenity: unexpected failure\n";
        let err = read_choice(&ran(255, "", stderr), exited_one_silently).unwrap_err();
        assert_eq!(err, "zenity: unexpected failure. Enter a path manually.");
    }

    /// Environment and programs a Linux picker test runs against.
    struct FakeDesktop {
        vars: Vec<(&'static str, &'static str)>,
        installed: Vec<(&'static str, Ran)>,
        calls: RefCell<Vec<(String, Vec<OsString>)>>,
    }

    impl FakeDesktop {
        fn pick(&self) -> Result<Option<PathBuf>, String> {
            pick_linux_with(
                |name| {
                    self.vars
                        .iter()
                        .find(|(key, _)| *key == name)
                        .map(|(_, value)| OsString::from(value))
                },
                |bin, args| {
                    self.calls
                        .borrow_mut()
                        .push((bin.to_string(), args.to_vec()));
                    self.installed
                        .iter()
                        .find(|(name, _)| *name == bin)
                        .map(|(_, ran)| ran.clone())
                },
            )
        }
    }

    #[test]
    fn test_pick_linux_with_zenity_choice_returns_folder() {
        let desk = FakeDesktop {
            vars: vec![("DISPLAY", ":0"), ("HOME", "/home/u")],
            installed: vec![("zenity", ran(0, "/home/u/My Notes\n", ""))],
            calls: RefCell::default(),
        };
        assert_eq!(
            desk.pick().unwrap(),
            Some(PathBuf::from("/home/u/My Notes"))
        );
        let calls = desk.calls.borrow();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "zenity");
    }

    #[test]
    fn test_pick_linux_with_zenity_cancel_and_gtk_noise_returns_none() {
        let desk = FakeDesktop {
            vars: vec![("WAYLAND_DISPLAY", "wayland-0")],
            installed: vec![(
                "zenity",
                ran(
                    1,
                    "",
                    "Gtk-Message: GtkDialog mapped without a transient parent\n",
                ),
            )],
            calls: RefCell::default(),
        };
        assert_eq!(desk.pick().unwrap(), None);
    }

    #[test]
    fn test_pick_linux_without_display_returns_error_and_runs_nothing() {
        let desk = FakeDesktop {
            vars: vec![("HOME", "/home/u"), ("DISPLAY", "")],
            installed: vec![("zenity", ran(1, "", ""))],
            calls: RefCell::default(),
        };
        let err = desk.pick().unwrap_err();
        assert!(err.contains("No desktop session"), "{err}");
        assert!(desk.calls.borrow().is_empty());
    }

    #[test]
    fn test_pick_linux_without_zenity_runs_kdialog_with_home_as_one_argument() {
        let desk = FakeDesktop {
            vars: vec![("DISPLAY", ":1"), ("HOME", "/home/u; rm -rf ~")],
            installed: vec![("kdialog", ran(0, "/home/u/src\n", ""))],
            calls: RefCell::default(),
        };
        assert_eq!(desk.pick().unwrap(), Some(PathBuf::from("/home/u/src")));
        let calls = desk.calls.borrow();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[1].0, "kdialog");
        assert_eq!(
            calls[1].1,
            [
                "--getexistingdirectory",
                "/home/u; rm -rf ~",
                "--title",
                "Choose a folder to pulp"
            ]
            .map(OsString::from)
        );
    }

    #[test]
    fn test_pick_linux_without_home_runs_kdialog_without_start_folder() {
        let desk = FakeDesktop {
            vars: vec![("DISPLAY", ":1")],
            installed: vec![("kdialog", ran(1, "", ""))],
            calls: RefCell::default(),
        };
        assert_eq!(desk.pick().unwrap(), None);
        let calls = desk.calls.borrow();
        assert_eq!(
            calls[1].1,
            [
                "--getexistingdirectory",
                "--title",
                "Choose a folder to pulp"
            ]
            .map(OsString::from)
        );
    }

    #[test]
    fn test_pick_linux_without_any_picker_returns_install_hint() {
        let desk = FakeDesktop {
            vars: vec![("DISPLAY", ":0")],
            installed: Vec::new(),
            calls: RefCell::default(),
        };
        let err = desk.pick().unwrap_err();
        assert!(err.contains("install zenity or kdialog"), "{err}");
    }

    #[test]
    fn test_windows_script_with_any_folder_writes_utf8() {
        let first = WINDOWS_SCRIPT
            .lines()
            .find(|line| !line.trim().is_empty())
            .unwrap();
        assert_eq!(
            first,
            "[Console]::OutputEncoding = [System.Text.Encoding]::UTF8"
        );
    }
}
