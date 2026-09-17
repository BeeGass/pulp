# pulp

Pulp grinds a local folder (or zip/tar) into one LLM-ready text file.

The name is a triple entendre: paper pulp; the verb *to pulp* (extract the juice); and pulp fiction — disposable reading you hand a model. It walks the tree in parallel, pulls text out of mixed documents, and writes one dump. Everything runs on the machine you point it at. Files never leave the machine.

## Install

```
cargo install --git https://github.com/BeeGass/pulp --locked
```

Requires Rust 1.85 or newer.

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
| `--include GLOB` | Repeatable allow-list |
| `--exclude GLOB` | Repeatable extra deny-list |
| `--no-default-excludes` | Do not apply the built-in deny-list |
| `--hidden` | Include hidden files (`.git` is still skipped) |
| `--no-gitignore` | Ignore `.gitignore` |
| `--follow-links` | Follow symlinks |
| `--archives` | Expand nested zip/tar |
| `--binaries` | Keep binary placeholders instead of skipping |
| `--notebook-outputs` | Include Jupyter cell outputs |
| `--tokens` | Token estimate in the summary |
| `--list` | Print relative paths only |
| `-q, --quiet` | No stderr summary |
| `ui` | Local mill at `http://127.0.0.1:8747` (`--port`, `--no-open`) |

## Web mill

`pulp ui` starts a letterpress-style mill on **127.0.0.1 only**. Click **Browse** to pick a folder in Finder (or the system file manager); the mill then scans it. Tick files (Rust, Lean, `.npz`, and the rest), pick `txt` / `md` / `xml`, then copy or download. The page talks to the same extractor as the CLI. Nothing is uploaded.

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
use pulp::{pack, Options, OutputFormat};

let opts = Options {
    roots: vec!["src".into()],
    format: OutputFormat::Markdown,
    ..Options::default()
};
let packed = pack(&opts)?;
```

`pack` never uploads anything. Render the dump with `pulp::render::write_all` using the same `Options.format`.

## License

MIT. See [LICENSE](LICENSE).
