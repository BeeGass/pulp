# Using pulp

The full guide to the `pulp` command and the local mill. For installation and a first run, start with the [README](../README.md).

- [Choosing what goes in](#choosing-what-goes-in)
- [Where the dump goes](#where-the-dump-goes)
- [Output formats](#output-formats)
- [The summary line, notes, and warnings](#the-summary-line-notes-and-warnings)
- [What gets pulped](#what-gets-pulped)
- [What stays out](#what-stays-out)
- [Budgets](#budgets)
- [The local mill](#the-local-mill)
- [The browser mill](#the-browser-mill)
- [Platforms and hardware](#platforms-and-hardware)
- [Using pulp as a library](#using-pulp-as-a-library)
- [Troubleshooting](#troubleshooting)

Every flag is in the README's [options table](../README.md#options), and `pulp --help` prints the same list.

## Choosing what goes in

```sh
pulp                           # the current folder
pulp src docs                  # several folders in one dump
pulp notes.pdf                 # a single file
pulp project.zip               # an archive: its members go in
pulp --list src                # the paths that would go in, without reading them
pulp --include '*.rs' .        # only Rust files
pulp --exclude 'tests/**' .    # everything except tests/
```

Paths can be folders, files, or archives, and default to the current folder. Each path is a root. With one root, the dump names files relative to it; with several, each path starts with its root's name, so `pulp src docs` prints `src/...` and `docs/...`.

`--include GLOB` keeps only matching files, and `--exclude GLOB` drops more. Both repeat, and `*.rs` also matches nested paths. `--hidden` lets hidden files in (`.git` is still skipped), and `--follow-links` follows symlinks. What stays out on its own is covered under [What stays out](#what-stays-out).

## Where the dump goes

```sh
pulp -o dump.xml .             # a file; the layout follows the extension
pulp -f md src                 # Markdown to stdout
pulp . | pbcopy                # straight to the clipboard on macOS
```

Omit `-o` to write to stdout (`-o -` does the same). If `-f` is omitted, the layout follows the `-o` extension (`.txt`, `.md`, `.xml`) and otherwise defaults to plain text.

The dump never packs itself: a file that `-o` names, or that stdout is redirected into, is left out of the walk and spends none of the budget. `-o` refuses a file that is one of the inputs, and checks before any work that its folder exists and can be written. The dump goes to a temporary file beside FILE (`.pulp-<pid>-<n>.tmp`) and replaces FILE only once it is complete, so a failed or interrupted run leaves an old FILE, or a symlink's target, as it was, and no temporary file. A FIFO or a device is written in place. If the reader of a pipe goes away (`pulp . | head`), pulp stops quietly.

## Output formats

| How to select | Layout |
| --- | --- |
| `-f txt` or `-o dump.txt` | A directory map, then a `FILE:` header before each file |
| `-f md` or `-o dump.md` | A directory map, then a `## path` heading and a fenced block per file |
| `-f xml` or `-o dump.xml` | `<documents>`, holding a `<document_tree>` and one `<document>` per file, with `<source>` and `<document_content>` |

`--tree` picks the directory map: `selected` (the default) maps the files in the dump, `full` also shows the files that were skipped, and `none` (or `--no-tree`) leaves the map out.

The same two files in plain text:

```text
Directory structure:
tides/
├── README.md
└── src/
    └── main.rs

================================================
FILE: README.md
================================================
# tides

Finds the high tide in a day of gauge readings.

================================================
FILE: src/main.rs
================================================
fn main() {
    println!("high tide: 4.02 m at 18:37");
}
```

And in XML:

```xml
<documents>
<document_tree>
tides/
├── README.md
└── src/
    └── main.rs

</document_tree>
<document index="1">
<source>README.md</source>
<document_content>
# tides

Finds the high tide in a day of gauge readings.

</document_content>
</document>
<document index="2">
<source>src/main.rs</source>
<document_content>
fn main() {
    println!("high tide: 4.02 m at 18:37");
}

</document_content>
</document>
</documents>
```

Names with control, bidi, or invisible format characters appear escaped (`\n`, `\u{202e}`) in headers, notes, the directory map, and `--list`, so a name cannot break a header or forge one. In XML, characters XML 1.0 cannot hold become U+FFFD.

## The summary line, notes, and warnings

Unless `--quiet`, pulp ends with a summary on stderr:

```text
pulped 2 files (115 B read, 161 chars, ~31 tokens) in 26ms
```

The token count is a quick estimate, not a tokenizer: about four characters per token, and one per Chinese, Japanese, or Korean character. `pulp -q --tokens .` prints the estimate alone, which helps when you are sizing a dump for a model's context window.

When some files do not make it in, the line ends with counts such as `, 1 unreadable, 2 skipped`.

- **Unreadable files.** A damaged or encrypted PDF, say, keeps a one-line note in the dump in place of its text. It is not counted as pulped and is left out of the directory map. So is an archive member that cannot be read out: unsupported compression, encryption, damage, or data shared with another member.
- **Mislabelled documents.** A PDF, Office file, or EPUB whose bytes are really a web page or plain text, as a failed download often leaves, is read as what it holds, under a note that says so: `[not a PDF: it holds an HTML page ("Preparing to download ...")]`. If it holds something else, an image say, it is unreadable, and its note names what it holds.
- **Skipped files.** Binary media gets no section at all. A file over `--max-file-size` keeps a one-line note.

Folders pulp cannot open, such as one without read permission or a symlink loop, print a `warning:` line each before the summary (the first ten, then a count); the rest of the tree is still pulped. `--list` ends with `listed N files` instead. `-q` hides the summary but not the warnings.

## What gets pulped

| Kind | Notes |
| --- | --- |
| Source | Rust, Lean, Python, TypeScript, TOML, Markdown, and other text |
| NumPy | `.npy` and `.npz` metadata and a small preview (not treated as binary) |
| HTML, XML, JSON | Readable text, or decoded source with `--source` |
| CSV, TSV | Tabular text |
| PDF, Word, PowerPoint, Excel, OpenDocument, EPUB, RTF | Extracted text |
| Jupyter | Cells; `--notebook-outputs` keeps outputs |
| Zip, tar | A root archive always expands; nested members need `--archives` |

Binary media (images, audio, wasm, and so on) is skipped unless `--binaries`, and so is a file whose name says text but whose bytes are binary: more than 1 in 32 of its first 8 KiB are NULs or control characters that text does not use. A few, as text copied out of a PDF often holds, do not make a file binary. `--list` leaves binaries out too, except such a file, since listing reads no file.

Extraction reads text a document already holds. A scanned PDF, one that is only page images, has no text layer, and pulp does not run OCR.

## What stays out

Noisy trees stay out even when they are tracked: `node_modules/`, `.next/`, `out/`, `runs/`, `target/`, `dist/`, `build/`, `toolchains/`, virtualenvs, `__pycache__/`, VCS folders, JavaScript lockfiles (`package-lock.json`, `yarn.lock`, `pnpm-lock.yaml`, `bun.lock`), minified `.js` and `.css`, `.env` files, keys (`*.pem`, `*.key`, `id_rsa`), object files and libraries, and similar. Rust `.rs`, Lean `.lean`, and NumPy arrays next to a skipped `target/` are still included. Other lockfiles, such as `Cargo.lock`, and generated paths that do show up go in from the CLI and start unticked in the mill.

Inside a git repository, pulp honors its `.gitignore` files, `.git/info/exclude`, and your global excludes, as `git status` does; `.ignore` files count anywhere. `--no-gitignore` turns all of them off. `--exclude GLOB` adds patterns and `--no-default-excludes` drops the built-in list. With several roots, the built-in list matches paths under each root, while your `--include` and `--exclude` globs match either that path or the path the dump prints (`secrets/**` or `app/secrets/**`).

## Budgets

| Flag | Default | Effect |
| --- | --- | --- |
| `--max-file-size SIZE` | `8MiB` | A larger file keeps a one-line note instead of its text |
| `--max-total-bytes SIZE` | `1GiB` | Stops at the first file that would take the total read past this |
| `--max-entries N` | `0` (no cap) | Stops after the first N files |

Sizes take suffixes such as `500k`, `1m`, `8MiB`, or `1GiB`. Budgets follow path order: depth first, each folder's entries sorted by name. The walk stops at the first file that does not fit `--max-entries` or `--max-total-bytes`, so the same tree always yields the same dump.

## The local mill

```sh
pulp ui
```

`pulp ui` opens a mill at `http://127.0.0.1:8747` (the next free port if 8747 is busy) in your browser, and prints its link, `open http://127.0.0.1:8747/?token=…`. The bare address shows a locked page, so other programs and other users on the machine cannot use the mill. **Browse** picks a folder in the OS file manager, or paste a path. Tick files, choose **Readable** or **Source** content and XML, Markdown, or plain text, then **Pulp**. **Try a sample** loads a small built-in project.

- A checkbox includes a file or a whole folder; the file name previews what pulp extracts; language chips tick every file of a kind; `/` filters.
- Changing format or the directory map redraws the last extraction. Other changes mark the dump out of date, and Copy and Download stay off until you pulp again.
- Files that could not be extracted are flagged in the tree. Select one to see why, untick it, or open a GitHub issue with the error text.
- A scan that could not read some folders (no read permission, say) names the first few and counts the rest.
- Pulping reads each ticked file as it is now, so a file edited since the scan goes in as edited. One that has disappeared or been swapped for a link, a folder, or a device is flagged as changed rather than read; scan again to pick up the folder as it is now.
- Pulping again reuses what the last pulp extracted from each file whose size and modification time have not changed since, so after an edit only the edited files are read and extracted again. A change of content, archives, or notebook outputs extracts every file afresh.
- One pack job at a time. Cancel (or Esc) asks it to stop between files. File previews run beside a pack and read only the chosen file from the last scan.
- Lockfiles, images, and virtualenv trees (`.venv/`, `venv/`) start unticked.
- The mill follows your OS light or dark setting and works on a phone.
- Nothing is uploaded. The page opens only from the printed link, and every API call carries its per-process session token.

```sh
pulp ui --port 9000 --no-open    # headless or remote; then open the printed link yourself
```

On a remote machine, run `pulp ui --no-open` there and open the printed `…/?token=…` link through a local forward on the same port: `ssh -L 8747:127.0.0.1:8747 host`.

## The browser mill

The in-browser mill at [pulp.onlygass.dev/mill](https://pulp.onlygass.dev/mill) packs files you choose or drop in the tab, with pulp built to WebAssembly. It shares its interface with `pulp ui` and produces the same dump for the same files. Nothing is uploaded. It has no `.gitignore` walk and no OS folder dialog, so `pulp ui` still wins for `.gitignore` discovery and typed paths. [wasm-mill.md](wasm-mill.md) compares the two mills in full.

## Platforms and hardware

Pulp is meant to feel the same on a laptop, a workstation, a Pi-class ARM board, or a headless box. It detects the OS and CPU count; there is nothing to configure.

| You are on | What you get |
| --- | --- |
| **macOS** (Apple Silicon or Intel) | **Browse** in `pulp ui` opens Finder. |
| **Linux** (x86_64 or aarch64) | **Browse** uses `zenity`, then `kdialog`. Install one of those for a graphical picker; otherwise type a path. Works on desktops, servers, and ARM boards (Raspberry Pi and similar). |
| **Windows** (x86_64 or ARM) | **Browse** opens the Explorer folder dialog. Paths can be typed if the dialog cannot open. Windows is not covered by CI yet. |
| **Headless or SSH** | Skip the browser with `pulp ui --no-open` and forward the port, as above, or stay on the CLI (`pulp -o dump.xml .`). |
| **Any other OS** | The CLI still packs if the crate builds. The graphical folder picker is macOS, Linux, or Windows only. |

Extraction uses as many threads as the OS reports, unless you pass `-j N`; the walk runs in one thread so its order never changes. A phone-class ARM board, a 4-core laptop, and a 32-thread desktop all work; more cores mainly shorten large trees.

PDF, Office, EPUB, and RTF extractors run in a child `pulp` process with a timeout, so a stuck parser does not take down the run or the mill; so does HTML whose nesting pulp cannot vouch for. HTML nested too deep to lay out is reduced to its text by a linear tag stripper.

## Using pulp as a library

Pulp is also a Rust crate. It is not on crates.io yet, so depend on the repository:

```toml
[dependencies]
pulp = { git = "https://github.com/BeeGass/pulp" }
```

```rust
use pulp::{Options, OutputFormat, Selection, pack, render};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let opts = Options {
        roots: vec!["src".into()],
        format: OutputFormat::Markdown,
        selection: Selection::AllEligible,
        ..Options::default()
    };
    let packed = pack(&opts)?;
    render::write_all(&mut std::io::stdout().lock(), &packed, &opts)?;
    Ok(())
}
```

- `Selection::Only(vec![])` matches nothing; it never becomes "all files", and an id is matched before a path.
- `Options::exclude` holds your own globs; the built-in list applies while `default_excludes` is on, and `exclude_globs()` gives both.
- `render::write_all` writes the dump in the chosen layout and leaves skipped binaries out.
- `scan_manifest` is the shared discovery step for scan, tree, and pack; `scan_manifest_with_warnings` also returns the paths the walk could not read.
- `pack_manifest` reads each file as it is now. `pack_manifest_cached` also takes the `ExtractCache` an earlier pack returned and reuses what it extracted from files whose path, size, and modification time are unchanged, for the same dump. The local mill keeps the cache of its last pack; the CLI reads every file.
- `apply_budgets` and `cmp_path_order` apply the same budgets in the same order to any file list.

The walk, the folder picker, and the mill need the default `native` feature. Without it (`default-features = false`), the crate still extracts and packs files you hand it in memory (`pack_entries`) and builds for `wasm32`, which is how the browser mill uses it.

## Troubleshooting

**`pulp: command not found` after installing.** Cargo installs into `~/.cargo/bin`. Open a new terminal, or run `source "$HOME/.cargo/env"`, so that folder is on your `PATH`.

**Building from a clone fails to link on macOS.** Inside a clone, `.cargo/config.toml` links with the Command Line Tools compiler. Install it with `xcode-select --install`, or install with `cargo install --git https://github.com/BeeGass/pulp --locked`, which does not read that file.

**Browse does nothing on Linux.** Install `zenity` or `kdialog`, or type the folder's path into the mill.

**A PDF comes out empty.** It is most likely a scan: pages stored as images with no text layer. Pulp does not run OCR. A PDF whose fonts carry no Unicode mapping, as some Chinese, Japanese, and Korean PDFs do, can also come out empty or fail to extract.

**A file is flagged unreadable.** It is damaged, encrypted, or not what its name says. The note in the dump says which. In the mill, select the file to see the reason, untick it, or report it.

**The dump is too big for my model.** Check its size with `pulp -q --tokens .`, then narrow it with `--include`, `--exclude`, or a subfolder, or cap it with `--max-total-bytes`.

**Port 8747 is taken.** `pulp ui` moves to the next free port and prints the link it chose. Pass `--port N` to pick one.
