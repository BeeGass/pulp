# Browser WASM mill — design

Status: planning. Landing lives at `pulp.onlygass.dev`; this doc describes the in-browser mill planned for `/mill`.

## Goals

- Pack a folder (or zip) into one LLM-ready dump **inside the browser tab**.
- Privacy parity with `pulp ui`: files never leave the device; no server-side upload/packer.
- Reuse as much of the existing Rust pack/extract/render pipeline as practical.
- Ship behind `pulp.onlygass.dev/mill` as a static + WASM asset set (no privileged backend).

## Non-goals

- Hosting the current axum mill on a public hostname (`src/ui.rs` binds `127.0.0.1` and rejects non-localhost Origin/Host by design).
- Accepting folder uploads to Vercel/Cloudflare/etc. for server-side packing.
- Feature-complete parity with every native extractor on day one (especially heavy Office/PDF).

## Current architecture (native)

- CLI + library: walk → classify → extract → render (`pack`, `extract`, `render`).
- Local mill: `pulp ui` serves `web/index.html` via axum, session token, localhost-only Origin/Host checks, APIs `/api/scan`, `/api/pack`, `/api/tree`, `/api/browse`.
- Folder pick uses a native picker (`pick`), not browser uploads.

The browser mill should call the **same pack core**, not reimplement formatting in JS.

## Proposed architecture

```
[ JS shell ]  File System Access API / <input webkitdirectory> / zip drop
      |
      v
[ wasm pulp-core ]  classify + extract + pack + render  (no axum, no filesystem crate assuming OS paths)
      |
      v
[ download blob ]  dump.txt / .md / .xml
```

- **JS shell**: UI (can start from a trimmed `web/index.html`), worker orchestration, file ingest, progress, download.
- **WASM core**: expose a narrow API, e.g. `scan(entries) -> manifest`, `pack(entries, options) -> { bytes, stats }`.
- **Ingest model**: virtual tree of `{ path, bytes }` (or streaming chunks). No `std::fs` in the wasm path; the host supplies bytes.
- **Workers**: run pack on a Web Worker (or `wasm-bindgen-rayon` later) so the UI stays responsive.

## Crate split (hypothesis — verify before locking)

| Layer | Likely wasm? | Notes |
| --- | --- | --- |
| `classify`, globs, ignores (adapted) | yes | Needs a virtual ignore source instead of walking `.gitignore` on disk |
| plain text / HTML / JSON / CSV / XML | yes | Low dependency risk |
| zip/tar (read members supplied as bytes) | yes / maybe | `zip`/`tar` crates often work; test size |
| PDF (`pdf-extract`) | maybe later | Pulls native-ish deps; may need feature-gate or JS-side pdf.js fallback |
| Office (`calamine`, docx/pptx path) | maybe later | Feature-gate out of MVP wasm |
| axum mill / opener / native folder pick | no | Stay native-only |

Preferred shape: `pulp` keeps the CLI + localhost mill; a `pulp-core` (or feature `wasm`) excludes UI and OS pickers and compiles to `wasm32-unknown-unknown` via `wasm-bindgen` / `wasm-pack`.

## Phased milestones

1. **MVP** — text/code/Markdown/JSON/CSV/HTML; drag-drop files + directory input; txt/md/xml out; hard size caps; ship placeholder UI replaced by working pack.
2. **Tree UX** — scan/manifest, include/exclude toggles mirroring CLI defaults, progress + cancel.
3. **Archives** — root zip in-browser; nested archives behind a flag.
4. **Heavy extractors** — PDF/Office as optional features or JS helpers; document fidelity gaps vs native.
5. **Polish** — OPFS caching, File System Access “open folder”, share target, installable PWA (still no network pack).

## Limits and budgets (starting points)

Mirror CLI defaults where possible (`max-file-size` 8MiB, `max-entries` 100k, `max-total-bytes` 1GiB) but **lower** for browser MVP (e.g. 2MiB/file, 50MiB total, 5k entries) until measured.

- WASM download gzip target: **< 2 MiB** for MVP feature set; measure after first `wasm-pack`.
- Pack of ~1k small source files: aim interactive (< a few seconds) on mid-tier laptops.

## Hosting

- Static assets under `site/mill/` (or `site/mill/app/` once built).
- COOP/COEP headers only if SharedArrayBuffer/threads are required; prefer single-thread MVP to avoid cross-origin isolation complexity.
- Never add a server route that accepts multipart folder uploads for packing.

## Security / privacy copy

Public messaging must stay accurate:

- Local CLI / `pulp ui`: files stay on the machine.
- Browser mill: files stay in the tab/device memory; no pulp server receives content.
- Analytics (if any) must not include path names or file contents.

## Open questions

1. Feature-gate vs separate `pulp-core` crate?
2. First extractor cut line for MVP (exclude PDF/Office entirely?).
3. Directory picker: File System Access only, or always offer `<input webkitdirectory>` fallback?
4. Do we port Rayon to wasm threads early, or stay single-threaded until MVP works?
5. How do we CI-test wasm (headless node + fixtures) without bloating native CI?

## References in-repo

- Local mill: `src/ui.rs`, `web/index.html`
- Pack pipeline: `src/pack.rs`, `src/extract/`, `src/render.rs`
- Landing: `site/`
