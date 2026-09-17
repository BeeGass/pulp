# pulp

Pulp grinds a local folder (or zip/tar) into one LLM-ready text file.

The name is a triple entendre: paper pulp; the verb *to pulp* (extract the juice); and pulp fiction — disposable reading you hand a model. It walks the tree in parallel, pulls text out of mixed documents, and writes one dump. Everything runs on the machine you point it at. Files never leave the machine.

## Install

```
cargo install --git https://github.com/BeeGass/pulp --locked
```

Requires Rust 1.85 or newer.


## Website

Product site: [pulp.onlygass.dev](https://pulp.onlygass.dev) (landing + install). Static sources live in [`site/`](site/).

The in-browser WASM mill is planned for `/mill` (see [`docs/wasm-mill.md`](docs/wasm-mill.md)). Until then, use the local mill:

```
pulp ui
```

## xtask (per machine / OS)

`cargo xtask` picks jobs and compiler env from the box you are on:

| Hostname | OS | Notes |
| --- | --- | --- |
| `Matrix` | macOS aarch64 | M1 Pro laptop. Uses Command Line Tools clang (Xcode license is unsigned). |
| `manifold` | Linux x86_64 | 9950X3D. Caps cargo at 24 jobs. Release builds use `target-cpu=native`. |
| `tensor` | Linux x86_64 | 3900X. Caps at 16 jobs. |
| `jacobian` | Linux aarch64 | Pi 5. Caps at 4 jobs. |
| `hessian` | Linux aarch64 | Pi 4. Caps at 2 jobs. |

```
cargo xtask doctor          # who we think this machine is
cargo xtask build           # debug
cargo xtask build --release
cargo xtask test
cargo xtask clippy
cargo xtask fmt
cargo xtask ui              # mill, with this host's env
cargo xtask run -- ui --no-open
```

Override the hostname with `PULP_HOST=manifold` if the kernel name is not the box name.

## Usage

```
pulp                  # stdout, plain txt
pulp -o dump.txt
pulp -o dump.md       # markdown inferred from extension
pulp -o dump.xml
pulp -f md src
pulp --archives project.zip
```

Omit `-o` to write to stdout. `-f` selects the dump layout; if `-f` is omitted, the layout is inferred from the `-o` extension (`.txt`, `.md`, `.xml`) and otherwise defaults to plain text.

```
pulp --list src                 # relative paths only
pulp --tree none -o dump.txt    # files, no directory map
pulp --hidden --no-gitignore .
pulp ui                         # local mill at http://127.0.0.1:8747
```

## Output formats

| How to select | Layout | Shape |
| --- | --- | --- |
| `-f txt` / `-f plain` / `-o dump.txt` | Plain | `FILE:` headers, optional directory map |
| `-f md` / `-f markdown` / `-o dump.md` | Markdown | `## path` headings and fenced code |
| `-f xml` / `-o dump.xml` | XML | Claude-style `<documents>` / `<document_content>` |

## What gets pulped

| Kind | Extensions | Notes |
| --- | --- | --- |
| Rust | `.rs` | Source text (not excluded by default) |
| Lean | `.lean` | Source text (not excluded by default) |
| NumPy | `.npy`, `.npz` | Array metadata and a small preview; not treated as binary |
| HTML | `.html`, `.htm` | Converted to text |
| XML | `.xml` | Tags stripped to readable text |
| JSON | `.json`, `.jsonl` | Pretty-printed when possible |
| CSV | `.csv`, `.tsv` | Tabular text |
| PDF | `.pdf` | Extracted text |
| Word | `.docx` | Document text |
| PowerPoint | `.pptx` | Slide text |
| Excel | `.xlsx` | Sheet text |
| OpenDocument | `.odt` | Document text |
| EPUB | `.epub` | Chapter text |
| RTF | `.rtf` | Converted to text |
| Jupyter | `.ipynb` | Cells; pass `--notebook-outputs` to keep outputs |
| Archives | `.zip`, `.tar`, `.tar.gz` | Root archives always expand; nested members need `--archives` |

Other text sources (Python, TypeScript, TOML, Markdown, …) are included as plain files. Binary media (images, audio, wasm, …) is skipped unless `--binaries`.

## Default excludes

Noisy trees and secrets stay out even when they are tracked or the folder is not a git repo:

`node_modules/`, `target/`, `dist/`, `build/`, virtualenvs, `__pycache__/`, VCS dirs, lockfiles, `.env` / `.env.*`, `*.pem`, `*.key`, object files, and similar.

**Not** excluded: Rust sources (`.rs`), Lean sources (`.lean`), NumPy arrays (`.npy`, `.npz`). `target/` is skipped; the `.rs` files next to it are not.

`--exclude GLOB` appends extra patterns. `--no-default-excludes` starts from an empty list, then applies `--exclude`. `.gitignore` is honored unless `--no-gitignore`.

## Options

| Flag | Meaning |
| --- | --- |
| `-o, --output FILE` | Write the dump here (default: stdout) |
| `-f, --format FMT` | `txt`/`plain`, `md`/`markdown`, `xml` |
| `--tree MODE` | `selected` (default), `full`, `none` |
| `--no-tree` | Same as `--tree none` |
| `-j, --jobs N` | Parallelism (`0` = Rayon default) |
| `--max-file-size SIZE` | Cap per file (default `8MiB`; `8000`, `8k`, `8KiB`, `8m`, `1g`) |
| `--max-entries N` | Cap discovered files (default `100000`) |
| `--max-total-bytes SIZE` | Cap summed input size (default `1GiB`) |
| `--include GLOB` | Repeatable allow-list |
| `--exclude GLOB` | Repeatable extra deny-list |
| `--no-default-excludes` | Do not apply the built-in deny-list |
| `--hidden` | Include hidden files (`.git` is still skipped) |
| `--no-gitignore` | Ignore `.gitignore` |
| `--follow-links` | Follow symlinks |
| `--archives` | Expand nested zip/tar |
| `--binaries` | Keep binary placeholders instead of skipping |
| `--notebook-outputs` | Include Jupyter cell outputs |
| `--source` | Keep HTML, XML, and JSON as source instead of converting |
| `--tokens` | Token estimate in the summary |
| `--list` | Print relative paths only |
| `-q, --quiet` | No stderr summary |
| `ui` | Local mill at `http://127.0.0.1:8747` (`--port`, `--no-open`) |

## Web mill

`pulp ui` starts a letterpress-style mill on **127.0.0.1 only**. A GitHub mark in the header links to the source at https://github.com/BeeGass/pulp. Click **Browse** to pick a folder in Finder (or the system file manager); the mill then scans it. Folders in the proof tree collapse. Tick the checkbox to include a file; click the filename to inspect a capped preview. **Content** is Preserve source or Readable text (HTML/XML/JSON only; PDFs and Office still convert). Dump format defaults to XML. Options holds gitignore, hidden, archives, notebook outputs, and no tree. Combined / File preview / Issues sit on the output pane. Copy and Download stay off while the dump is out of date. POSTs carry a per-process session token. Nothing is uploaded. Virtualenv trees (`.venv/`, `venv/`), lockfiles, and images start unchecked.

```
pulp ui
pulp ui --port 9000 --no-open
```

Unless `--quiet`, stderr looks like:

```
pulped 12 files (48.2 KiB text, ~12100 tokens) in 35ms
```

Walk uses a parallel gitignore walker (`ignore`); extraction uses Rayon.

## Library

```rust
use pulp::{pack, Options, OutputFormat, Selection};

let opts = Options {
    roots: vec!["src".into()],
    format: OutputFormat::Markdown,
    selection: Selection::AllEligible,
    ..Options::default()
};
let packed = pack(&opts)?;
```

`Selection::Only(vec![])` matches nothing; it never becomes “all files”. `pack` never uploads anything. Render the dump with `pulp::render::write_all` using the same `Options.format`. `scan_manifest` is the shared discovery step for scan, tree, and pack.

## License

MIT. See [LICENSE](LICENSE).
