---
title: "pulp — LLM-ready dumps from local folders"
description: "Pulp grinds a local folder into one LLM-ready text file. Runs on your machine. Browser mill at /mill — files stay in the tab."
url: "https://pulp.onlygass.dev/"
markdown: "https://pulp.onlygass.dev/index.md"
author: "Bryan Gass"
---

# Pulp

Grind a folder into one **LLM-ready dump**.

Pulp walks a tree in parallel, pulls text out of code, PDFs, Office files, and notebooks, and writes one file you can hand a model.

**Local first.** Nothing is uploaded. The CLI mill (`pulp ui`) binds localhost only. The [browser mill](https://pulp.onlygass.dev/mill) packs in WebAssembly inside your tab. Both mills have a **Try a sample** key that loads a small built-in project.

## Install

```bash
cargo install --git https://github.com/BeeGass/pulp --locked
pulp ui              # local mill on 127.0.0.1:8747
pulp -o dump.xml .   # this folder, one XML dump
pulp -f md src       # Markdown to stdout
```

Requires Rust 1.85 or newer.

## What goes in

| Kind | Notes |
| --- | --- |
| Source | Rust, Lean, Python, TypeScript, TOML, Markdown, and other text |
| NumPy | `.npy` and `.npz` metadata and a small preview (not treated as binary) |
| HTML, XML, JSON | Readable text, or decoded source with `--source` |
| CSV, TSV | Tabular text |
| PDF, Word, PowerPoint, Excel, OpenDocument, EPUB, RTF | Extracted text |
| Jupyter | Cells; `--notebook-outputs` keeps outputs |
| Zip, tar | A root archive always expands; nested members need `--archives` |

Binary media is skipped unless `--binaries`. Noisy trees stay out even when tracked: `node_modules/`, `target/`, `dist/`, virtualenvs, lockfiles, `.env`, keys, and similar. `.gitignore` is honored unless `--no-gitignore`.

## Formats

Plain text (`-f txt`), Markdown (`-f md`), or Claude-style XML (`-f xml`). Without `-f`, the layout follows the `-o` extension and otherwise defaults to plain text.

## Options

| Flag | Meaning |
| --- | --- |
| `-o, --output FILE` | Write the dump here (default: stdout) |
| `-f, --format FMT` | `txt`, `md`, or `xml` |
| `--tree MODE` | `selected` (default), `full`, or `none` |
| `-j, --jobs N` | Parallelism; `0` uses every available core |
| `--include GLOB` | Repeatable allow-list |
| `--exclude GLOB` | Repeatable extra deny-list |
| `--hidden` | Include hidden files (`.git` is still skipped) |
| `--no-gitignore` | Ignore `.gitignore` and `.ignore` |
| `--archives` | Expand nested zip and tar members |
| `--source` | Keep HTML, XML, and JSON as source |
| `--notebook-outputs` | Include Jupyter cell outputs |
| `--list` | Print relative paths only |

Every flag is in the [README](https://github.com/BeeGass/pulp#options).

## Two mills

| | Browser mill (`/mill`) | Local mill (`pulp ui`) |
| --- | --- | --- |
| Files leave the device | Never | Never |
| Choosing files | Folder or file picker, or drop | OS file manager, or any typed path |
| `.gitignore` | Not available | Honored |
| Default excludes | Same core matcher | Same core matcher |
| PDF, Office, zip | In-process WebAssembly | A child process with a timeout |
| Cancel | Stops the worker | Between files |
| Setup | None | One cargo line |

In-browser mill: [/mill](https://pulp.onlygass.dev/mill) · [mill.md](https://pulp.onlygass.dev/mill.md)

## Agent maps

- [llms.txt](https://pulp.onlygass.dev/llms.txt)
- [llms-full.txt](https://pulp.onlygass.dev/llms-full.txt)
- [sitemap.xml](https://pulp.onlygass.dev/sitemap.xml)

## Source

https://github.com/BeeGass/pulp
