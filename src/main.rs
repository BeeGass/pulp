use std::fs::{self, File};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
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

fn main() -> ExitCode {
    // Heavy parsers run in a child `pulp` whatever this executable is named.
    pulp::extract::isolate::set_isolation(true);
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        // A reader such as `head` closed the pipe; the rest of the dump has
        // nowhere to go, which is not an error.
        Err(err) if is_broken_pipe(&err) => ExitCode::SUCCESS,
        Err(err) => {
            // Like every stderr write, this one may meet a closed stderr,
            // which must not turn into a panic.
            let _ = writeln!(io::stderr(), "Error: {err:?}");
            ExitCode::FAILURE
        }
    }
}

fn is_broken_pipe(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause
            .downcast_ref::<io::Error>()
            .is_some_and(|io| io.kind() == io::ErrorKind::BrokenPipe)
    })
}

fn run(cli: Cli) -> anyhow::Result<()> {
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
    let output = prepare_output(cli.output.as_deref(), &cli.paths)?;
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
        skip_paths: output.skip_paths,
        skip_identities: output.skip_identities,
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
        let mut out = io::stdout().lock();
        for file in &packed.files {
            writeln!(out, "{}", display_path(&file.relative))?;
        }
    } else {
        output
            .dest
            .write(|mut w| pulp::render::write_all(&mut w, &packed, &opts))?;
    }
    // Walk warnings print even with --quiet, which hides only the summary.
    // A closed stderr is no reason to fail a dump that was written.
    let mut err = io::stderr().lock();
    for line in warning_lines(&warnings) {
        let _ = writeln!(err, "{line}");
    }
    if !opts.quiet {
        let _ = writeln!(err, "{}", summary_line(&packed));
    }
    Ok(())
}

/// Where the dump goes and what the walk must leave out because of it.
struct Output {
    dest: Destination,
    /// The destination as given and as it resolves, so an old dump inside a
    /// scanned folder is not packed into the new one.
    skip_paths: Vec<PathBuf>,
    /// Device and inode numbers of the destination, and of stdout when the
    /// dump goes there, taken from open descriptors where a path cannot
    /// name the file.
    skip_identities: Vec<(u64, u64)>,
}

/// Where the dump is written.
enum Destination {
    Stdout,
    /// A regular file, or a new one: the dump goes to a temporary file
    /// beside it, which is renamed over it only once complete. A failed
    /// write leaves the old file whole, and a symlink to it stays a symlink.
    Replace(PathBuf),
    /// A device, or another name for an open descriptor (`/dev/null`, a
    /// terminal, `/dev/fd/3`): opened before the walk and written in place.
    Device(File, PathBuf),
    /// A FIFO, written in place. It is opened only once the dump is ready,
    /// since opening a FIFO for writing waits for a reader.
    Fifo(PathBuf),
}

/// Settle where the dump goes before any work is done, so a destination
/// that cannot take it fails now rather than after the pack.
fn prepare_output(out: Option<&Path>, roots: &[PathBuf]) -> anyhow::Result<Output> {
    let Some(out) = out else {
        // `pulp . > dump.txt`: the shell made the dump before pulp ran, so
        // the walk would find it. Leave it out rather than pack the dump
        // into itself.
        return Ok(Output {
            dest: Destination::Stdout,
            skip_paths: Vec::new(),
            skip_identities: stdout_identity().into_iter().collect(),
        });
    };
    let resolved = follow_links(out)?;
    let mut skip_paths = vec![out.to_path_buf(), resolved.target.clone()];
    if let Ok(cwd) = std::env::current_dir() {
        skip_paths.push(cwd.join(out));
    }
    let skip_paths = pulp::walk::normalize_skip_paths(&skip_paths);
    if resolved.stdout {
        return Ok(Output {
            dest: Destination::Stdout,
            skip_paths,
            skip_identities: stdout_identity().into_iter().collect(),
        });
    }
    let (dest, identity) = match output_kind(fs::metadata(out), resolved.descriptor) {
        OutputKind::Directory => anyhow::bail!("output {} is a directory", out.display()),
        OutputKind::File(meta) => {
            refuse_input(out, &meta, roots)?;
            check_writable_dir(&resolved.target)?;
            (Destination::Replace(resolved.target), identity_of(&meta))
        }
        OutputKind::New => {
            check_writable_dir(&resolved.target)?;
            (Destination::Replace(resolved.target), None)
        }
        OutputKind::Fifo => (Destination::Fifo(out.to_path_buf()), None),
        OutputKind::InPlace => {
            let device = fs::OpenOptions::new()
                .append(true)
                .open(out)
                .with_context(|| format!("open {}", out.display()))?;
            let identity = device.metadata().ok().and_then(|meta| identity_of(&meta));
            (Destination::Device(device, out.to_path_buf()), identity)
        }
    };
    Ok(Output {
        dest,
        skip_paths,
        skip_identities: identity.into_iter().collect(),
    })
}

/// What `-o` names, as its metadata tells.
enum OutputKind {
    /// Nothing yet, or its directory is a file; the directory check says
    /// which.
    New,
    Directory,
    /// A regular file, replaced whole.
    File(fs::Metadata),
    Fifo,
    /// Anything else, written in place: a device, another name for an open
    /// descriptor, or a path whose metadata cannot be read at all, as for
    /// `NUL` and `CON` on Windows. Opening it reports any real problem.
    InPlace,
}

/// How to write to a path whose metadata came back as `meta`. `descriptor`
/// says the path named an open descriptor on its way (`/dev/fd/3`).
fn output_kind(meta: io::Result<fs::Metadata>, descriptor: bool) -> OutputKind {
    match meta {
        Err(err)
            if matches!(
                err.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
            ) =>
        {
            OutputKind::New
        }
        Err(_) => OutputKind::InPlace,
        Ok(meta) if meta.is_dir() => OutputKind::Directory,
        Ok(meta) if meta.is_file() && !descriptor => OutputKind::File(meta),
        Ok(meta) if is_fifo(&meta) => OutputKind::Fifo,
        Ok(_) => OutputKind::InPlace,
    }
}

/// Refuse an output that is one of the input files: writing it would
/// destroy the input.
fn refuse_input(out: &Path, meta: &fs::Metadata, roots: &[PathBuf]) -> anyhow::Result<()> {
    for root in roots {
        let Ok(root_meta) = fs::metadata(root) else {
            continue;
        };
        if root_meta.is_file() && same_file(out, meta, root, &root_meta) {
            anyhow::bail!(
                "output {} is also an input; writing it would destroy the input",
                out.display()
            );
        }
    }
    Ok(())
}

/// Prove the directory that will hold `target` exists and takes a new file,
/// by creating one there and removing it at once.
fn check_writable_dir(target: &Path) -> anyhow::Result<()> {
    let dir = parent_dir(target);
    match fs::metadata(dir) {
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => anyhow::bail!("{} is not a directory", dir.display()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            anyhow::bail!("output directory {} does not exist", dir.display())
        }
        Err(err) if err.kind() == io::ErrorKind::NotADirectory => {
            anyhow::bail!("{} is not a directory", dir.display())
        }
        Err(err) => {
            return Err(err).with_context(|| format!("output directory {}", dir.display()));
        }
    }
    match TempFile::create_beside(target, true) {
        Ok((probe, file)) => {
            drop(file);
            drop(probe);
            Ok(())
        }
        Err(err) => anyhow::bail!(
            "cannot create a file in output directory {}: {err}",
            dir.display()
        ),
    }
}

/// The directory `path` would live in.
fn parent_dir(path: &Path) -> &Path {
    match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir,
        _ => Path::new("."),
    }
}

/// A temporary file beside the output. Dropping it removes the file unless
/// it was renamed into place, so an early return or a panic leaves nothing
/// behind; on Unix, so does a signal that ends the run while it exists.
struct TempFile {
    path: PathBuf,
    kept: bool,
    #[cfg(unix)]
    _on_signal: signals::RemoveOnSignal,
}

impl TempFile {
    /// Create a new file, `.pulp-{pid}-{n}.tmp`, in the directory that
    /// holds `target`. The name stays short however long the target's is.
    /// With `private`, only its owner can read it until it is given the
    /// mode of the file it replaces.
    fn create_beside(target: &Path, private: bool) -> io::Result<(Self, File)> {
        let dir = parent_dir(target);
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        if private {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        #[cfg(not(unix))]
        let _ = private;
        #[cfg(unix)]
        let on_signal = signals::RemoveOnSignal::install();
        for attempt in 0..100u32 {
            let path = dir.join(format!(".pulp-{}-{attempt}.tmp", std::process::id()));
            // Registered before it is created, so the file never exists
            // unregistered. A signal before then unlinks nothing, or a stale
            // file of this name, which carries this process's id.
            #[cfg(unix)]
            on_signal.register(&path);
            match options.open(&path) {
                Ok(file) => {
                    let temp = Self {
                        path,
                        kept: false,
                        #[cfg(unix)]
                        _on_signal: on_signal,
                    };
                    return Ok((temp, file));
                }
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
                Err(err) => return Err(err),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "no unused temporary name",
        ))
    }

    /// Rename the file over `target`. Only then is it kept.
    fn rename_to(mut self, target: &Path) -> io::Result<()> {
        fs::rename(&self.path, target)?;
        self.kept = true;
        Ok(())
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        if !self.kept {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// Removing the temporary file when a signal ends the run (Unix).
///
/// While a temporary file exists, SIGINT, SIGTERM, and SIGHUP run a handler
/// that unlinks it, restores the default action, and raises the signal
/// again, so the run still ends the way the signal asked and the exit
/// status stays conventional. A signal the run started out ignoring, as
/// under `nohup`, stays ignored.
#[cfg(unix)]
mod signals {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;
    use std::ptr;
    use std::sync::atomic::{AtomicPtr, Ordering};

    const SIGNALS: [libc::c_int; 3] = [libc::SIGINT, libc::SIGTERM, libc::SIGHUP];

    /// The file the handler unlinks, as a NUL-terminated path, or null. A
    /// path stored here is never freed, since a handler running on another
    /// thread may still be reading it. A run stores one per name it tries,
    /// usually two: the probe and the dump.
    static TEMP_PATH: AtomicPtr<libc::c_char> = AtomicPtr::new(ptr::null_mut());

    /// The handler, installed for as long as this lives. Dropping it puts
    /// back the actions it replaced and forgets the registered path.
    pub(super) struct RemoveOnSignal {
        replaced: Vec<(libc::c_int, libc::sigaction)>,
    }

    impl RemoveOnSignal {
        pub(super) fn install() -> Self {
            let handler = on_signal as extern "C" fn(libc::c_int);
            let mut replaced = Vec::new();
            for signal in SIGNALS {
                // SAFETY: an all-zero `sigaction` is a valid value, `sigaction`
                // only reads and writes the two structs on this stack frame,
                // and `on_signal` does only async-signal-safe work.
                unsafe {
                    let mut old: libc::sigaction = std::mem::zeroed();
                    if libc::sigaction(signal, ptr::null(), &mut old) != 0
                        || old.sa_sigaction == libc::SIG_IGN
                    {
                        continue;
                    }
                    let mut action: libc::sigaction = std::mem::zeroed();
                    action.sa_sigaction = handler as libc::sighandler_t;
                    libc::sigemptyset(&mut action.sa_mask);
                    if libc::sigaction(signal, &action, ptr::null_mut()) == 0 {
                        replaced.push((signal, old));
                    }
                }
            }
            Self { replaced }
        }

        /// Have the handler unlink `path`.
        pub(super) fn register(&self, path: &Path) {
            if let Ok(path) = CString::new(path.as_os_str().as_bytes()) {
                TEMP_PATH.store(path.into_raw(), Ordering::SeqCst);
            }
        }
    }

    impl Drop for RemoveOnSignal {
        fn drop(&mut self) {
            for (signal, old) in &self.replaced {
                // SAFETY: `old` is the action `sigaction` reported for `signal`.
                unsafe { libc::sigaction(*signal, old, ptr::null_mut()) };
            }
            TEMP_PATH.store(ptr::null_mut(), Ordering::SeqCst);
        }
    }

    /// Unlink the registered temporary file, if there is one. Safe inside a
    /// signal handler: it is an atomic load and `unlink`.
    pub(super) fn remove_registered_temp() {
        let path = TEMP_PATH.load(Ordering::SeqCst);
        if !path.is_null() {
            // SAFETY: `path` came from `CString::into_raw` and is never freed.
            unsafe { libc::unlink(path) };
        }
    }

    extern "C" fn on_signal(signal: libc::c_int) {
        remove_registered_temp();
        // SAFETY: `signal` and `raise` are async-signal-safe. With the
        // default action back, the raised signal ends the run as soon as
        // this handler returns.
        unsafe {
            libc::signal(signal, libc::SIG_DFL);
            libc::raise(signal);
        }
    }
}

impl Destination {
    /// Write the dump with `write`. Only a temporary file this run made is
    /// ever removed; the destination itself never is.
    fn write(self, write: impl FnOnce(&mut dyn Write) -> io::Result<()>) -> anyhow::Result<()> {
        match self {
            Self::Stdout => {
                let mut w = BufWriter::with_capacity(64 * 1024, io::stdout().lock());
                write(&mut w)?;
                w.flush()?;
            }
            Self::Replace(target) => replace_file(&target, write)?,
            Self::Device(file, path) => {
                let mut w = BufWriter::with_capacity(64 * 1024, file);
                write(&mut w)
                    .and_then(|()| w.flush())
                    .with_context(|| format!("write {}", path.display()))?;
            }
            Self::Fifo(path) => {
                let fifo = fs::OpenOptions::new()
                    .write(true)
                    .open(&path)
                    .with_context(|| format!("open {}", path.display()))?;
                let mut w = BufWriter::with_capacity(64 * 1024, fifo);
                write(&mut w)
                    .and_then(|()| w.flush())
                    .with_context(|| format!("write {}", path.display()))?;
            }
        }
        Ok(())
    }
}

/// Write a new file beside `target` and rename it over `target`. The new
/// file takes the old one's permissions. An error, a panic, or a signal
/// that ends the run removes the new file and nothing else, so the old one
/// is left as it was (see [`TempFile`]).
fn replace_file(
    target: &Path,
    write: impl FnOnce(&mut dyn Write) -> io::Result<()>,
) -> anyhow::Result<()> {
    let old = fs::metadata(target).ok().filter(fs::Metadata::is_file);
    let (temp, file) = TempFile::create_beside(target, old.is_some())
        .with_context(|| format!("create a file beside {}", target.display()))?;
    let written = (|| -> io::Result<()> {
        let mut w = BufWriter::with_capacity(64 * 1024, file);
        write(&mut w)?;
        let file = w.into_inner().map_err(io::IntoInnerError::into_error)?;
        if let Some(old) = &old {
            // Some mounts (FAT, some network shares) refuse a mode change;
            // the dump still belongs in place.
            let _ = file.set_permissions(old.permissions());
        }
        drop(file);
        temp.rename_to(target)
    })();
    written.with_context(|| format!("write {}", target.display()))
}

/// Where an output path leads.
struct Resolved {
    /// The file the path names once the symlinks at its end are followed.
    target: PathBuf,
    /// Whether a step of the way named an open descriptor (`/dev/fd/3`).
    descriptor: bool,
    /// Whether a step of the way named this process's stdout.
    stdout: bool,
}

/// Follow the symlinks at the end of `path`, noting which steps name an
/// open descriptor rather than a file.
fn follow_links(path: &Path) -> anyhow::Result<Resolved> {
    let mut resolved = Resolved {
        target: path.to_path_buf(),
        descriptor: false,
        stdout: false,
    };
    for _ in 0..40 {
        resolved.descriptor |= is_descriptor_path(&resolved.target);
        resolved.stdout |= is_stdout_path(&resolved.target);
        match fs::symlink_metadata(&resolved.target) {
            Ok(meta) if meta.file_type().is_symlink() => {
                let link = fs::read_link(&resolved.target)
                    .with_context(|| format!("read link {}", resolved.target.display()))?;
                resolved.target = match resolved.target.parent() {
                    Some(dir) => dir.join(link),
                    None => link,
                };
            }
            _ => return Ok(resolved),
        }
    }
    anyhow::bail!("output {} has too many levels of symlinks", path.display())
}

/// Whether `path` names an open descriptor rather than a file, as
/// `/dev/stdout`, `/dev/fd/3`, and `/proc/self/fd/3` do.
#[cfg(unix)]
fn is_descriptor_path(path: &Path) -> bool {
    let path = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    path.starts_with("/dev/fd")
        || ["/dev/stdin", "/dev/stdout", "/dev/stderr"]
            .iter()
            .any(|name| path == Path::new(name))
        || (path.starts_with("/proc") && path.components().any(|part| part.as_os_str() == "fd"))
}

#[cfg(not(unix))]
fn is_descriptor_path(_path: &Path) -> bool {
    false
}

/// Whether `path` names this process's stdout.
#[cfg(unix)]
fn is_stdout_path(path: &Path) -> bool {
    let path = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    ["/dev/stdout", "/dev/fd/1", "/proc/self/fd/1"]
        .iter()
        .any(|name| path == Path::new(name))
}

#[cfg(not(unix))]
fn is_stdout_path(_path: &Path) -> bool {
    false
}

#[cfg(unix)]
fn is_fifo(meta: &fs::Metadata) -> bool {
    use std::os::unix::fs::FileTypeExt;
    meta.file_type().is_fifo()
}

#[cfg(not(unix))]
fn is_fifo(_meta: &fs::Metadata) -> bool {
    false
}

/// Whether two paths name one file: the same device and inode on Unix, the
/// same canonical path elsewhere.
#[cfg(unix)]
fn same_file(_a: &Path, a: &fs::Metadata, _b: &Path, b: &fs::Metadata) -> bool {
    identity_of(a) == identity_of(b)
}

#[cfg(not(unix))]
fn same_file(a: &Path, _: &fs::Metadata, b: &Path, _: &fs::Metadata) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

/// Device and inode of a file.
#[cfg(unix)]
fn identity_of(meta: &fs::Metadata) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    Some((meta.dev(), meta.ino()))
}

#[cfg(not(unix))]
fn identity_of(_meta: &fs::Metadata) -> Option<(u64, u64)> {
    None
}

/// Device and inode of stdout when it is a regular file (a shell
/// redirect). Read from the open descriptor: on macOS, `stat` on
/// `/dev/stdout` reports a different device than the file's own.
#[cfg(unix)]
fn stdout_identity() -> Option<(u64, u64)> {
    use std::os::fd::AsFd;
    let fd = io::stdout().as_fd().try_clone_to_owned().ok()?;
    let meta = File::from(fd).metadata().ok()?;
    if meta.is_file() {
        identity_of(&meta)
    } else {
        None
    }
}

#[cfg(not(unix))]
fn stdout_identity() -> Option<(u64, u64)> {
    None
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
    fn test_is_broken_pipe_with_wrapped_io_error_returns_true() {
        let pipe = anyhow::Error::from(io::Error::from(io::ErrorKind::BrokenPipe)).context("write");
        assert!(is_broken_pipe(&pipe));
        let other = anyhow::Error::from(io::Error::from(io::ErrorKind::NotFound));
        assert!(!is_broken_pipe(&other));
    }

    /// Serializes the tests that make temporary files: the path a signal
    /// handler would remove is one per process.
    fn temp_files_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[test]
    fn test_temp_file_with_no_rename_is_removed_on_drop_or_unwind() {
        let _lock = temp_files_lock();
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("dump.txt");
        let count = || fs::read_dir(dir.path()).unwrap().count();

        let (temp, file) = TempFile::create_beside(&target, false).unwrap();
        drop(file);
        assert_eq!(count(), 1);
        drop(temp);
        assert_eq!(count(), 0);

        let unwound = std::panic::catch_unwind(|| {
            let (_temp, _file) = TempFile::create_beside(&target, false).unwrap();
            std::panic::resume_unwind(Box::new("the write failed"));
        });
        assert!(unwound.is_err());
        assert_eq!(count(), 0);

        let (temp, mut file) = TempFile::create_beside(&target, false).unwrap();
        file.write_all(b"dump\n").unwrap();
        drop(file);
        temp.rename_to(&target).unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "dump\n");
        assert_eq!(count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn test_remove_registered_temp_with_temp_file_alive_unlinks_it() {
        let _lock = temp_files_lock();
        let dir = tempfile::tempdir().unwrap();
        let (temp, file) = TempFile::create_beside(&dir.path().join("dump.txt"), false).unwrap();
        drop(file);
        let path = temp.path.clone();
        // What the signal handler does before it raises the signal again.
        signals::remove_registered_temp();
        assert!(!path.exists());
        drop(temp);
        // Once the guard is gone, a signal removes nothing.
        fs::write(&path, b"another file now").unwrap();
        signals::remove_registered_temp();
        assert!(path.exists());
    }

    #[test]
    fn test_create_beside_with_250_byte_target_name_returns_short_temp_name() {
        let _lock = temp_files_lock();
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join(format!("{}.txt", "d".repeat(246)));
        let (temp, _file) = TempFile::create_beside(&target, false).unwrap();
        let name = temp
            .path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert!(
            name.starts_with(".pulp-") && name.ends_with(".tmp"),
            "{name}"
        );
        assert!(name.len() <= 32, "{name}");
    }

    #[test]
    fn test_output_kind_with_each_metadata_error_returns_new_or_in_place() {
        use io::ErrorKind;
        for kind in [ErrorKind::NotFound, ErrorKind::NotADirectory] {
            let got = output_kind(Err(io::Error::from(kind)), false);
            assert!(matches!(got, OutputKind::New), "{kind:?}");
        }
        // Windows cannot read `NUL` or `CON` metadata; such a path is written
        // in place, and opening it reports any real problem.
        for kind in [
            ErrorKind::PermissionDenied,
            ErrorKind::InvalidInput,
            ErrorKind::Unsupported,
            ErrorKind::Other,
        ] {
            let got = output_kind(Err(io::Error::from(kind)), false);
            assert!(matches!(got, OutputKind::InPlace), "{kind:?}");
        }
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("dump.txt");
        fs::write(&file, "old\n").unwrap();
        let kind = |path: &Path, descriptor| output_kind(fs::metadata(path), descriptor);
        assert!(matches!(kind(dir.path(), false), OutputKind::Directory));
        assert!(matches!(kind(&file, false), OutputKind::File(_)));
        assert!(matches!(kind(&file, true), OutputKind::InPlace));
        #[cfg(unix)]
        {
            assert!(matches!(
                kind(Path::new("/dev/null"), false),
                OutputKind::InPlace
            ));
            let fifo = dir.path().join("pipe");
            let made = std::process::Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .unwrap();
            assert!(made.success());
            assert!(matches!(kind(&fifo, false), OutputKind::Fifo));
        }
    }

    #[test]
    fn test_prepare_output_with_input_file_missing_dir_or_file_parent_returns_error() {
        let _lock = temp_files_lock();
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("notes.txt");
        fs::write(&input, "keep me\n").unwrap();
        let roots = [dir.path().to_path_buf(), input.clone()];
        let message = |out: &Path| match prepare_output(Some(out), &roots) {
            Ok(_) => String::new(),
            Err(err) => err.to_string(),
        };
        assert!(message(&input).contains("also an input"));
        let missing = dir.path().join("no/such");
        assert_eq!(
            message(&missing.join("dump.txt")),
            format!("output directory {} does not exist", missing.display())
        );
        assert_eq!(
            message(&input.join("dump.txt")),
            format!("{} is not a directory", input.display())
        );
        assert!(message(dir.path()).ends_with("is a directory"));

        let fine = dir.path().join("dump.txt");
        let output = prepare_output(Some(&fine), &roots).unwrap();
        assert!(matches!(output.dest, Destination::Replace(ref path) if *path == fine));
        let names: Vec<_> = fs::read_dir(dir.path()).unwrap().collect();
        assert_eq!(names.len(), 1, "the writability probe must not stay behind");
    }

    #[cfg(unix)]
    #[test]
    fn test_prepare_output_with_descriptor_paths_writes_in_place() {
        let null = prepare_output(Some(Path::new("/dev/null")), &[]).unwrap();
        assert!(matches!(null.dest, Destination::Device(..)));
        let stdout = prepare_output(Some(Path::new("/dev/stdout")), &[]).unwrap();
        assert!(matches!(stdout.dest, Destination::Stdout));
        assert!(is_descriptor_path(Path::new("/dev/fd/3")));
        assert!(is_descriptor_path(Path::new("/proc/self/fd/3")));
        assert!(!is_descriptor_path(Path::new("/dev/shm/dump.txt")));
        assert!(!is_descriptor_path(Path::new("fd/3")));
    }

    #[cfg(unix)]
    #[test]
    fn test_prepare_output_with_symlink_replaces_its_target() {
        let _lock = temp_files_lock();
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real.txt");
        fs::write(&real, "old\n").unwrap();
        let link = dir.path().join("link.txt");
        std::os::unix::fs::symlink("real.txt", &link).unwrap();
        let output = prepare_output(Some(&link), &[]).unwrap();
        assert!(matches!(output.dest, Destination::Replace(ref path) if *path == real));
        output.dest.write(|w| w.write_all(b"new\n")).unwrap();
        assert_eq!(fs::read_to_string(&real).unwrap(), "new\n");
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
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
