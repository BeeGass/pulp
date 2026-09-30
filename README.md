# pulp

Grind a local folder into one LLM-ready dump.

Pulp walks a tree, pulls text out of mixed documents in parallel, and writes a single file you can hand a model. The name is a triple entendre: paper pulp; the verb *to pulp* (extract the juice); and pulp fiction — disposable reading.

**Files never leave the machine.** There is no upload, no account, and no cloud packer. The mill binds `127.0.0.1` only.

Product site: [pulp.onlygass.dev](https://pulp.onlygass.dev) · Source: [github.com/BeeGass/pulp](https://github.com/BeeGass/pulp)

```
cargo install --git https://github.com/BeeGass/pulp --locked
pulp ui
pulp -o dump.xml .
```

Requires [Rust](https://rustup.rs/) 1.88 or newer.

---

## Local mill

```
pulp ui
```

Opens a localhost mill at `http://127.0.0.1:8747` (the next free port if 8747 is busy) in your browser, and prints its link, `open http://127.0.0.1:8747/?token=…`. The bare address shows a locked page, so other programs and other users on the machine cannot use the mill. Browse picks a folder in the OS file manager, or paste a path. Tick files, choose **Readable** or **Source** content and XML / Markdown / plain text, then Pulp. **Try a sample** loads a small built-in project.

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

```
pulp ui --port 9000 --no-open    # headless or remote; then open the printed link yourself
```

The in-browser mill at [pulp.onlygass.dev/mill](https://pulp.onlygass.dev/mill) packs files you choose or drop in this tab (no gitignore walk, no OS folder dialog). It shares its interface with `pulp ui` and produces the same dump for the same files; `pulp ui` still wins for `.gitignore` discovery and typed paths.

---

## On your machine

Pulp is meant to feel the same on a laptop, a workstation, a Pi-class ARM board, or a headless box. It detects OS and CPU count; you do not need a named host.

| You are on | What you get |
| --- | --- |
| **macOS** (Apple Silicon or Intel) | `pulp ui` Browse opens Finder. Building from source uses the Command Line Tools compiler when that SDK is present, so an unsigned Xcode license does not block the link. |
| **Linux** (x86_64 or aarch64) | Browse uses `zenity`, then `kdialog`. Install one of those for a graphical picker; otherwise type a path. Works on desktops, servers, and ARM boards (Raspberry Pi and similar). |
| **Windows** (x86_64 or ARM) | Browse opens the Explorer folder dialog. Paths can be typed if the dialog cannot open. |
| **Headless / SSH** | Skip the browser with `pulp ui --no-open` and open the printed `…/?token=…` link through a local forward on the same port (`ssh -L 8747:127.0.0.1:8747 host`), or stay on the CLI (`pulp -o dump.xml .`). |
| **Any other OS** | The CLI still packs if the crate builds. The graphical folder picker is macOS, Linux, or Windows only. |

**Hardware.** Extraction uses as many threads as the OS reports, unless you pass `-j N`; the walk runs in one thread so its order never changes. A phone-class ARM board, a 4-core laptop, and a 32-thread desktop all work; more cores mainly shorten large trees. Release builds on macOS and Linux can use the host CPU (`cargo xtask build --release`). PDF, Office, EPUB, and RTF extractors run in a child `pulp` process with a timeout so a stuck parser does not take down the mill; so does HTML whose nesting pulp cannot vouch for. HTML nested too deep to lay out is reduced to its text by a linear tag stripper.

**Building from source.** `cargo xtask` sets job count from available parallelism and, on macOS, prefers Command Line Tools clang when it is installed. Named-lab overrides exist for a few boxes; everyone else is `unknown` and still gets a sensible default. `PULP_HOST` is only needed if you are deliberately pretending to be one of those boxes.

```
cargo xtask doctor    # OS, arch, detected jobs
cargo xtask build
cargo xtask test
cargo xtask ui
cargo xtask site      # copy the shared mill UI from web/ into site/
cargo xtask ui-test   # mill browser tests; needs Chrome or Chromium
```

The mill's interface lives in `web/` (`pulp.css`, `mill.css`, `mill.js`, and the sample project). `pulp ui` embeds it; `cargo xtask site` copies it into `site/` for the browser mill, and `cargo xtask test` fails if the copies drift. Its browser tests live in `web/test/`: `cargo xtask ui-test` runs them, along with the landing page and the browser mill from `site/`, in headless Chrome or Chromium (`PULP_CHROME` picks the binary; `--serve` serves the pages to your own browser instead). It also builds pulp and runs `web/test/local.test.html` against a real `pulp ui`, which it starts on a free port and stops afterwards.

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

The dump never packs itself: a file that `-o` names, or that stdout is redirected into, is left out of the walk and spends none of the budget. `-o` refuses a file that is one of the inputs, and checks before any work that its folder exists and can be written. The dump goes to a temporary file beside FILE (`.pulp-<pid>-<n>.tmp`) and replaces FILE only once it is complete, so a failed or interrupted run leaves an old FILE, or a symlink's target, as it was, and no temporary file. A FIFO or a device is written in place. If the reader of a pipe goes away (`pulp . | head`), pulp stops quietly.

| How to select | Layout |
| --- | --- |
| `-f txt` / `-o dump.txt` | `FILE:` headers and an optional directory map |
| `-f md` / `-o dump.md` | `## path` headings and fenced code |
| `-f xml` / `-o dump.xml` | `<documents>` / `<document_content>` |

Unless `--quiet`, stderr looks like:

```
pulped 12 files (48.2 KiB read, 12100 chars, ~12100 tokens) in 35ms
```

When some files do not make it in, the line ends with counts such as `, 1 unreadable, 2 skipped`. An unreadable file (a damaged or encrypted PDF, say) keeps a one-line note in the dump in place of its text; it is not counted as pulped and is left out of the directory map. So does an archive member that cannot be read out: unsupported compression, encryption, damage, or data shared with another member. A PDF, Office file, or EPUB whose bytes are really a web page or plain text, as a failed download often leaves, is read as what it holds, under a note that says so: `[not a PDF: it holds an HTML page ("Preparing to download ...")]`. If it holds something else, an image say, it is unreadable, and its note names what it holds. Skipped binary media gets no section at all; a file over `--max-file-size` keeps a one-line note.

Folders pulp cannot open, such as one without read permission or a symlink loop, print a `warning:` line each before the summary (the first ten, then a count); the rest of the tree is still pulped. `--list` ends with `listed N files` instead. `-q` hides the summary but not the warnings, and with `--tokens` prints the estimate alone.

Names with control, bidi, or invisible format characters appear escaped (`\n`, `\u{202e}`) in headers, notes, the directory map, and `--list`, so a name cannot break a header or forge one. In XML, characters XML 1.0 cannot hold become U+FFFD.

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

Binary media (images, audio, wasm, …) is skipped unless `--binaries`, and so is a file whose name says text but whose bytes are binary: more than 1 in 32 of its first 8 KiB are NULs or control characters that text does not use. A few, as text copied out of a PDF often holds, do not make a file binary. `--list` leaves binaries out too, except such a file, since listing reads no file.

Noisy trees stay out even when they are tracked: `node_modules/`, `.next/`, `out/`, `runs/`, `target/`, `dist/`, `build/`, `toolchains/`, virtualenvs, `__pycache__/`, VCS dirs, lockfiles, `.env`, keys, object files, and similar. Rust `.rs`, Lean `.lean`, and NumPy arrays next to a skipped `target/` are still included. Generated paths that do show up start unchecked.

Inside a git repository, pulp honors its `.gitignore` files, `.git/info/exclude`, and your global excludes, as `git status` does; `.ignore` files count anywhere. `--no-gitignore` turns all of them off. `--exclude GLOB` adds patterns and `--no-default-excludes` drops the built-in list. With several roots, the built-in list matches paths under each root, while your `--include` and `--exclude` globs match either that path or the path the dump prints (`secrets/**` or `app/secrets/**`).

Budgets follow path order: depth first, each folder's entries sorted by name. The walk stops at the first file that does not fit `--max-entries` or `--max-total-bytes`, so the same tree always yields the same dump.

---

## Options

| Flag | Meaning |
| --- | --- |
| `-o, --output FILE` | Write the dump here (default: stdout, as is `-o -`) |
| `-f, --format FMT` | `txt` / `md` / `xml` |
| `--tree MODE` | `selected` (default), `full`, `none` |
| `--no-tree` | Same as `--tree none` |
| `-j, --jobs N` | Extraction threads (`0` = all available cores) |
| `--max-file-size SIZE` | Cap per file (default `8MiB`) |
| `--max-entries N` | Keep only the first N files in path order (`0`, the default, means no cap) |
| `--max-total-bytes SIZE` | Cap summed input, filled in path order (default `1GiB`) |
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
| `--tokens` | With `-q`, print only the token estimate (not with `--list`) |
| `--list` | Print the paths that would be pulped (to `-o` when given) |
| `-q, --quiet` | No stderr summary; warnings still print |
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

`Selection::Only(vec![])` matches nothing; it never becomes “all files”, and an id is matched before a path. `Options::exclude` holds your own globs; the built-in list applies while `default_excludes` is on, and `exclude_globs()` gives both. Render with `pulp::render::write_all`, which leaves skipped binaries out. `scan_manifest` is the shared discovery step for scan, tree, and pack; `scan_manifest_with_warnings` also returns the paths the walk could not read. `pack_manifest` reads each file as it is now; `pack_manifest_cached` also takes the `ExtractCache` an earlier pack returned and reuses what it extracted from files whose path, size, and modification time are unchanged, for the same dump. The local mill keeps the cache of its last pack; the CLI reads every file. `apply_budgets` and `cmp_path_order` apply the same budgets in the same order to any file list.

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
