# pulp

[![CI](https://github.com/BeeGass/pulp/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/BeeGass/pulp/actions/workflows/ci.yml?query=branch%3Amain)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Rust 1.88+](https://img.shields.io/badge/rust-1.88%2B-orange.svg)](https://www.rust-lang.org/tools/install)

**Grind a local folder into one LLM-ready dump.**

Pulp walks a folder, pulls the text out of everything in it (code, PDFs, Word, PowerPoint, and Excel files, notebooks, archives) in parallel, and writes a single file you can hand a language model. Run it from the command line, from a local web app, or in your browser with nothing to install.

**Your files never leave your machine.** There is no upload, no account, and no cloud service. The local mill listens on `127.0.0.1` only, and the browser mill works inside the tab.

The name is a triple entendre: paper pulp; the verb *to pulp*, to extract the juice; and pulp fiction, disposable reading.

[Website](https://pulp.onlygass.dev) · [Browser mill](https://pulp.onlygass.dev/mill) · [Usage guide](docs/usage.md) · [Report a bug](https://github.com/BeeGass/pulp/issues)

## Contents

- [Features](#features)
- [Installation](#installation)
- [Quick start](#quick-start)
- [Common tasks](#common-tasks)
- [Output formats](#output-formats)
- [What goes in](#what-goes-in)
- [Options](#options)
- [The two mills](#the-two-mills)
- [Use it from Rust](#use-it-from-rust)
- [Privacy and security](#privacy-and-security)
- [Documentation](#documentation)
- [Contributing](#contributing)
- [Citation](#citation)
- [License](#license)

## Features

- **One file out.** Plain text, Markdown, or XML, with a map of the folder on top, ready to paste into a chat or attach to a prompt.
- **Reads real folders.** Source code and text in any language, plus PDF, Word, PowerPoint, Excel, OpenDocument, EPUB, RTF, HTML, XML, JSON, CSV, Jupyter notebooks, NumPy arrays, and zip or tar archives.
- **Leaves the noise out.** Honors `.gitignore`, and skips `node_modules/`, build output, virtualenvs, caches, secrets such as `.env` files and keys, and binary media.
- **Same folder, same dump.** Files go in path order, and size budgets stop at the same file every run.
- **Fast.** Extraction runs on every core. In `pulp ui`, pulping again re-reads only the files that changed.
- **Hard to break.** A damaged or encrypted file gets a one-line note and the run goes on; a mislabelled one is read as what it really holds. Parsers that could hang run in a child process with a time limit.
- **Three ways to run it.** A CLI for scripts, a point-and-click mill on localhost (`pulp ui`), and the same mill in your browser, built to WebAssembly.

## Installation

Pulp is written in Rust and installs with Cargo. It needs Rust 1.88 or newer.

**1. Install Rust** (skip this if `cargo --version` already works):

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

On Windows, use the installer from [rustup.rs](https://rustup.rs). To update an existing Rust, run `rustup update stable`.

**2. Install pulp:**

```sh
cargo install --git https://github.com/BeeGass/pulp --locked
```

Cargo downloads the source, builds it (a few minutes the first time), and puts `pulp` in `~/.cargo/bin`, which rustup adds to your `PATH`.

**3. Check that it works:**

```sh
pulp --version
```

```text
pulp 0.1.0
```

### Updating and uninstalling

To update, run the install command again; it rebuilds from the latest commit on `main` (add `--force` if Cargo says pulp is already installed). To remove pulp:

```sh
cargo uninstall pulp
```

### Building from a clone

```sh
git clone https://github.com/BeeGass/pulp
cd pulp
cargo install --path . --locked
```

On macOS, a build inside the clone links with the Command Line Tools; install them with `xcode-select --install` if you have not. [docs/development.md](docs/development.md) covers the rest of the build tooling.

### Platform notes

| Platform | Notes |
| --- | --- |
| macOS (Apple Silicon or Intel) | **Browse** in the mill opens Finder. |
| Linux (x86_64 or aarch64) | **Browse** uses `zenity`, then `kdialog`; install one for a folder dialog, or type a path. Runs on desktops, servers, and ARM boards such as a Raspberry Pi. |
| Windows (x86_64 or ARM) | **Browse** opens the Explorer folder dialog. Windows is not covered by CI yet. |
| Headless or SSH | Use the CLI, or run `pulp ui --no-open` and open its link through an SSH port forward ([how](docs/usage.md#the-local-mill)). |

### No install at all

The [browser mill](https://pulp.onlygass.dev/mill) runs the same packer inside your browser tab. Choose or drop a folder, tick what goes in, and copy or download the dump. Nothing is uploaded.

## Quick start

Point pulp at a folder. Take this small project:

```text
tides/
├── README.md
└── src/
    └── main.rs
```

<details>
<summary>Create it to follow along</summary>

```sh
mkdir -p tides/src
printf '# tides\n\nFinds the high tide in a day of gauge readings.\n' > tides/README.md
printf 'fn main() {\n    println!("high tide: 4.02 m at 18:37");\n}\n' > tides/src/main.rs
```

</details>

```sh
pulp -f md tides
```

Pulp prints one Markdown document: a map of the folder, then each file under its own heading.

````markdown
# Directory structure

```
tides/
├── README.md
└── src/
    └── main.rs
```

## README.md

```markdown
# tides

Finds the high tide in a day of gauge readings.
```

## src/main.rs

```rust
fn main() {
    println!("high tide: 4.02 m at 18:37");
}
```
````

A summary goes to stderr, so it never lands in the dump:

```text
pulped 2 files (115 B read, 161 chars, ~31 tokens) in 26ms
```

To save the dump instead, name a file. The layout follows its extension:

```sh
pulp -o tides.xml tides
```

Then paste or attach the file wherever you talk to your model.

### Or click through it

```sh
pulp ui
```

This opens the local mill in your browser, on `127.0.0.1:8747` or the next free port. **Browse** to a folder (or paste its path), tick the files you want, pick a format, and press **Pulp**, then copy or download the result. **Try a sample** loads a small built-in project if you want to look around first.

## Common tasks

```sh
pulp                               # the current folder, plain text to stdout
pulp -o dump.xml .                 # the current folder into dump.xml
pulp src docs -o dump.md           # several folders in one Markdown dump
pulp --list .                      # what would go in, without reading anything
pulp --include '*.py' .            # only Python files
pulp --exclude 'tests/**' .        # everything except tests/
pulp -q --tokens .                 # only the token estimate
pulp --archives project.zip        # an archive, and the archives inside it
pulp --notebook-outputs analysis/  # keep Jupyter cell outputs
pulp . | pbcopy                    # straight to the clipboard on macOS
```

## Output formats

| Format | Select with | Layout |
| --- | --- | --- |
| Plain text (default) | `-f txt` or `-o dump.txt` | A directory map, then a `FILE:` header before each file |
| Markdown | `-f md` or `-o dump.md` | A directory map, then a `## path` heading and a fenced block per file |
| XML | `-f xml` or `-o dump.xml` | `<documents>` with one `<document>` per file (`<source>`, `<document_content>`) |

`--tree none` drops the directory map. The [usage guide](docs/usage.md#output-formats) shows each layout in full.

## What goes in

| Kind | What you get |
| --- | --- |
| Source code and text | As is: Rust, Python, TypeScript, Lean, TOML, Markdown, and any other text |
| PDF, Word, PowerPoint, Excel, OpenDocument, EPUB, RTF | The document's text |
| HTML, XML, JSON | Readable text, or the source with `--source` |
| CSV, TSV | Tables as text |
| Jupyter notebooks | Cells; `--notebook-outputs` keeps outputs |
| NumPy `.npy`, `.npz` | Type and shape, and a small preview |
| Zip, tar | The archive's members; archives nested inside need `--archives` |

Images, audio, and other binary files are skipped (`--binaries` keeps a placeholder for each). Inside a git repository, pulp honors `.gitignore` as `git status` does. Built-in excludes keep out dependency and build folders, virtualenvs, caches, JavaScript lockfiles, minified files, `.env` files, and keys; `--no-default-excludes` turns them off. Pulp does not run OCR, so a scanned PDF with no text layer comes out empty. The [usage guide](docs/usage.md#what-gets-pulped) has the details.

## Options

`pulp [OPTIONS] [PATHS]...` packs the given files, folders, or archives (default: the current folder).

| Flag | Meaning |
| --- | --- |
| `-o, --output FILE` | Write the dump here (default: stdout, as is `-o -`) |
| `-f, --format FMT` | `txt`, `md`, or `xml` (default: from the `-o` extension, else `txt`) |
| `--tree MODE` | Directory map: `selected` (default), `full`, or `none` |
| `--no-tree` | Same as `--tree none` |
| `-j, --jobs N` | Extraction threads (`0`, the default, uses every core) |
| `--max-file-size SIZE` | Cap per file (default `8MiB`) |
| `--max-entries N` | Keep only the first N files in path order (`0`, the default, means no cap) |
| `--max-total-bytes SIZE` | Cap summed input, filled in path order (default `1GiB`) |
| `--include GLOB` | Repeatable allow-list |
| `--exclude GLOB` | Repeatable extra deny-list |
| `--no-default-excludes` | Do not apply the built-in deny-list |
| `--hidden` | Include hidden files (`.git` is still skipped) |
| `--no-gitignore` | Ignore `.gitignore` and `.ignore` |
| `--follow-links` | Follow symlinks |
| `--archives` | Expand nested zip and tar members |
| `--binaries` | Keep binary placeholders instead of skipping |
| `--notebook-outputs` | Include Jupyter cell outputs |
| `--source` | Keep HTML, XML, and JSON as source |
| `--tokens` | With `-q`, print only the token estimate (not with `--list`) |
| `--list` | Print the paths that would be pulped (to `-o` when given) |
| `-q, --quiet` | No stderr summary; warnings still print |
| `-V, --version` | Print the version |
| `-h, --help` | Print help |

`pulp ui` opens the local mill:

| Flag | Meaning |
| --- | --- |
| `-p, --port PORT` | Port on localhost (default `8747`, or the next free port if it is busy) |
| `--no-open` | Print the link instead of opening a browser |

## The two mills

The mill is pulp's point-and-click interface. It comes in two builds that share one interface and produce the same dump for the same files.

| | Local mill (`pulp ui`) | Browser mill ([/mill](https://pulp.onlygass.dev/mill)) |
| --- | --- | --- |
| Setup | Install pulp | None |
| Choosing files | OS folder dialog, or any typed path | Folder or file picker, or drag and drop |
| `.gitignore` | Honored | Not read |
| Built-in excludes | Applied | Applied |
| Where it runs | A server on `127.0.0.1` only | WebAssembly inside the tab |
| Files uploaded | Never | Never |

[docs/wasm-mill.md](docs/wasm-mill.md) compares them in detail.

## Use it from Rust

Pulp is also a library. It is not on crates.io yet, so depend on the repository:

```toml
[dependencies]
pulp = { git = "https://github.com/BeeGass/pulp" }
```

```rust
use pulp::{Options, OutputFormat, pack, render};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let opts = Options {
        roots: vec!["src".into()],
        format: OutputFormat::Markdown,
        ..Options::default()
    };
    let packed = pack(&opts)?;
    render::write_all(&mut std::io::stdout().lock(), &packed, &opts)?;
    Ok(())
}
```

The [usage guide](docs/usage.md#using-pulp-as-a-library) covers selections, caching, and budgets.

## Privacy and security

- Pulp reads your files and writes the dump. It makes no network requests of its own and has no telemetry.
- The local mill binds `127.0.0.1` only. Its page opens only from the link `pulp ui` prints, and every request carries a per-process session token, so other programs and other users on the machine cannot use it.
- The browser mill works on files inside the tab. Nothing is sent to a server.
- Secrets such as `.env` files and private keys stay out by default. Check what goes in with `pulp --list` before you share a dump.
- A dump is written to a temporary file and moved into place once complete, so an interrupted run never leaves half a dump behind.

## Documentation

- [Usage guide](docs/usage.md): every option, the mill, output details, platforms, the library, and troubleshooting
- [Browser mill](docs/wasm-mill.md): how the in-browser mill works and how it differs from `pulp ui`
- [Development](docs/development.md): building from source, running the checks, and the website
- [pulp.onlygass.dev](https://pulp.onlygass.dev): the project's website

## Contributing

Bug reports and pull requests are welcome. If a file will not extract, the mill's **Report** button opens an issue with the error text filled in; otherwise [open an issue](https://github.com/BeeGass/pulp/issues/new) with the command you ran and what you expected. Work lands on the `dev` branch, and [docs/development.md](docs/development.md) lists the checks to run before a pull request.

## Citation

If you use pulp in research, please cite it. GitHub's **Cite this repository** button reads [CITATION.cff](CITATION.cff), or use this BibTeX:

```bibtex
@software{gass_pulp_2026,
  author  = {Gass, Bryan},
  title   = {pulp: Grind a local folder into one LLM-ready dump},
  year    = {2026},
  version = {0.1.0},
  url     = {https://github.com/BeeGass/pulp},
  license = {MIT}
}
```

## License

Pulp is released under the [MIT License](LICENSE). Copyright (c) 2026 Bryan Gass.

It includes third-party work under its own terms:

- `vendor/pdf-extract/`: a patched copy of [pdf-extract](https://github.com/jrmuizel/pdf-extract) by Jeff Muizelaar, MIT License.
- `web/fonts/`: IBM Plex Sans and a subset of IBM Plex Mono, SIL Open Font License 1.1 ([details](web/fonts/LICENSE.md)).
