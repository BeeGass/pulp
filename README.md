# pulp

Grind a local folder into one LLM-ready dump.

Pulp walks a tree in parallel, pulls text out of mixed documents, and writes a single file you can hand a model. The name is a triple entendre: paper pulp; the verb *to pulp* (extract the juice); and pulp fiction — disposable reading.

**Files never leave the machine.** There is no upload, no account, and no cloud packer. The mill binds `127.0.0.1` only.

Product site: [pulp.onlygass.dev](https://pulp.onlygass.dev) · Source: [github.com/BeeGass/pulp](https://github.com/BeeGass/pulp)

```
cargo install --git https://github.com/BeeGass/pulp --locked
pulp ui
pulp -o dump.xml .
```

Requires [Rust](https://rustup.rs/) 1.85 or newer.

---

## Local mill

```
pulp ui
```

Opens a localhost mill at `http://127.0.0.1:8747` (the next free port if 8747 is busy). Browse picks a folder in the OS file manager. Tick files, choose **Preserve source** or **Readable text**, pick XML / Markdown / plain text, then Pulp.

- Checkbox includes a file; the filename inspects a capped preview; the twist expands a folder.
- Changing format or the directory map re-renders the last extraction. Copy and Download stay off while the dump is out of date.
- One pack job at a time. Cancel asks it to stop between files.
- Lockfiles, images, and virtualenv trees (`.venv/`, `venv/`) start unchecked.
- Nothing is uploaded. POSTs carry a per-process session token.

```
pulp ui --port 9000 --no-open    # headless or remote; then open the URL yourself
```

An in-browser WASM mill is planned for [pulp.onlygass.dev/mill](https://pulp.onlygass.dev/mill). Until then, use `pulp ui` on the machine that has the files.

---

## On your machine

Pulp is meant to feel the same on a laptop, a workstation, a Pi-class ARM board, or a headless box. It detects OS and CPU count; you do not need a named host.

| You are on | What you get |
| --- | --- |
| **macOS** (Apple Silicon or Intel) | `pulp ui` Browse opens Finder. Building from source uses the Command Line Tools compiler when that SDK is present, so an unsigned Xcode license does not block the link. |
| **Linux** (x86_64 or aarch64) | Browse uses `zenity`, then `kdialog`. Install one of those for a graphical picker; otherwise type a path. Works on desktops, servers, and ARM boards (Raspberry Pi and similar). |
| **Windows** (x86_64 or ARM) | Browse opens the Explorer folder dialog. Paths can be typed if the dialog cannot open. |
| **Headless / SSH** | Skip the browser with `pulp ui --no-open` and open `http://127.0.0.1:…` from a local forward, or stay on the CLI (`pulp -o dump.xml .`). |
| **Any other OS** | The CLI still packs if the crate builds. The graphical folder picker is macOS, Linux, or Windows only. |

**Hardware.** Walk and extract use as many threads as the OS reports, unless you pass `-j N`. A phone-class ARM board, a 4-core laptop, and a 32-thread desktop all work; more cores mainly shorten large trees. Release builds on macOS and Linux can use the host CPU (`cargo xtask build --release`). PDF, Office, EPUB, and RTF extractors run in a child `pulp` process with a timeout so a stuck parser does not take down the mill.

**Building from source.** `cargo xtask` sets job count from available parallelism and, on macOS, prefers Command Line Tools clang when it is installed. Named-lab overrides exist for a few boxes; everyone else is `unknown` and still gets a sensible default. `PULP_HOST` is only needed if you are deliberately pretending to be one of those boxes.

```
cargo xtask doctor    # OS, arch, detected jobs
cargo xtask build
cargo xtask test
cargo xtask ui
```

---

## CLI

```
pulp                         # cwd → stdout, plain text
pulp -o dump.xml .
pulp -o dump.md src          # markdown from the extension
pulp -f md src
pulp --archives project.zip
pulp --list src
pulp --tree none -o dump.txt
```

Omit `-o` to write to stdout. If `-f` is omitted, layout follows the `-o` extension (`.txt`, `.md`, `.xml`) and otherwise defaults to plain text.

| How to select | Layout |
| --- | --- |
| `-f txt` / `-o dump.txt` | `FILE:` headers and an optional directory map |
| `-f md` / `-o dump.md` | `## path` headings and fenced code |
| `-f xml` / `-o dump.xml` | `<documents>` / `<document_content>` |

Unless `--quiet`, stderr looks like:

```
pulped 12 files (48.2 KiB read, 12100 chars, ~12100 tokens) in 35ms
```

---

## What gets pulped

| Kind | Notes |
| --- | --- |
| Source | Rust, Lean, Python, TypeScript, TOML, Markdown, and other text |
| NumPy | `.npy` / `.npz` metadata and a small preview (not treated as binary) |
| HTML / XML / JSON | Readable text, or decoded source with `--source` |
| CSV / TSV | Tabular text |
| PDF, Word, PowerPoint, Excel, OpenDocument, EPUB, RTF | Extracted text |
| Jupyter | Cells; `--notebook-outputs` keeps outputs |
| Zip / tar | A root archive always expands; nested members need `--archives` |

Binary media (images, audio, wasm, …) is skipped unless `--binaries`.

Noisy trees stay out even when they are tracked: `node_modules/`, `target/`, `dist/`, `build/`, virtualenvs, `__pycache__/`, VCS dirs, lockfiles, `.env`, keys, object files, and similar. Rust `.rs`, Lean `.lean`, and NumPy arrays next to a skipped `target/` are still included.

`.gitignore` is honored unless `--no-gitignore`. `--exclude GLOB` adds patterns. `--no-default-excludes` starts from an empty deny list.

---

## Options

| Flag | Meaning |
| --- | --- |
| `-o, --output FILE` | Write the dump here (default: stdout) |
| `-f, --format FMT` | `txt` / `md` / `xml` |
| `--tree MODE` | `selected` (default), `full`, `none` |
| `--no-tree` | Same as `--tree none` |
| `-j, --jobs N` | Parallelism (`0` = all available cores) |
| `--max-file-size SIZE` | Cap per file (default `8MiB`) |
| `--max-entries N` | Cap discovered files (default `100000`) |
| `--max-total-bytes SIZE` | Cap summed input (default `1GiB`) |
| `--include GLOB` | Repeatable allow-list |
| `--exclude GLOB` | Repeatable extra deny-list |
| `--no-default-excludes` | Do not apply the built-in deny-list |
| `--hidden` | Include hidden files (`.git` is still skipped) |
| `--no-gitignore` | Ignore `.gitignore` / `.ignore` |
| `--follow-links` | Follow symlinks |
| `--archives` | Expand nested zip/tar |
| `--binaries` | Keep binary placeholders instead of skipping |
| `--notebook-outputs` | Include Jupyter cell outputs |
| `--source` | Keep HTML, XML, and JSON as source |
| `--tokens` | Token estimate in the summary |
| `--list` | Print relative paths only |
| `-q, --quiet` | No stderr summary |
| `ui` | Local mill (`--port`, `--no-open`) |

---

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

`Selection::Only(vec![])` matches nothing; it never becomes “all files”. Render with `pulp::render::write_all`. `scan_manifest` is the shared discovery step for scan, tree, and pack.

---

## Branches

Work lands on `dev`. `main` is the published line and only moves when `dev` is merged into it — not by a direct push.

```
git checkout dev
git pull
# …commits…
git push origin dev
```

Then open a pull request from `dev` into `main` and merge it.

---

## License

MIT. See [LICENSE](LICENSE).
