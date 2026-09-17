# Browser WASM mill

Status: **MVP shipped** at https://pulp.onlygass.dev/mill

## Goals

- Pack a folder (or files) into one LLM-ready dump **inside the browser tab**.
- Privacy parity with `pulp ui`: files never leave the device; no server-side upload/packer.
- Ship behind `pulp.onlygass.dev/mill` as static HTML + WASM.

## What shipped (MVP)

- Crate: `crates/pulp-wasm` (`wasm-bindgen`), artifacts in `site/mill/pkg/`.
- UI: `site/mill/index.html` — folder/files picker, drag-drop, txt/md/xml, download.
- Extractors: UTF-8 text/code/Markdown, JSON pretty-print, HTML tag strip.
- Caps: 2 MiB/file, 50 MiB total, 5 000 entries.
- Dump layouts match pulp plain / markdown / XML shapes.

## Deferred

- Shared `pulp` crate pack path (feature-gated) instead of a sibling crate.
- PDF / Office / EPUB / archives in-browser.
- File System Access API “open folder” beyond `<input webkitdirectory>`.
- Web Worker + progress/cancel.
- Tree/manifest UX and include/exclude toggles.

## Rebuild

On macOS with Command Line Tools (same env as `cargo xtask` on Matrix):

```bash
export DEVELOPER_DIR=/Library/Developer/CommandLineTools
export SDKROOT=/Library/Developer/CommandLineTools/SDKs/MacOSX.sdk
export CC=$DEVELOPER_DIR/usr/bin/clang
export CXX=$DEVELOPER_DIR/usr/bin/clang++

wasm-pack build crates/pulp-wasm --target web --release --out-dir ../../site/mill/pkg
rm -f site/mill/pkg/.gitignore   # keep artifacts tracked for Vercel
```

Serve `site/` locally to verify, then push — Vercel project **pulp-landing** deploys Root Directory `site`.

## Non-goals (still)

- Hosting the axum mill on a public hostname.
- Accepting folder uploads to a server for packing.

## Full mill (2026-09-17)

The browser mill now links against `pulp` with `default-features = false` (no axum/CLI/fs walk). Extractors that compile on `wasm32-unknown-unknown` run in-process:

- text / code / markdown / JSON / HTML / XML / CSV / notebooks
- PDF (`pdf-extract`), Office spreadsheets (`calamine`), RTF
- zip archives (deflate) via the same pack expand path as native

Still native-only: subprocess isolation timeouts for hang-prone parsers, `.gitignore` filesystem walks, folder picker dialogs that need the OS mill.

Artifacts: `site/mill/pkg/` (~3.3 MiB wasm) served statically from Vercel root `site`.
