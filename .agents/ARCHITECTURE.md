# Architecture

Map of this checkout for agents. The human docs ([`README.md`](../README.md), [`docs/`](../docs/)) win if they disagree.

Pulp walks a local tree, extracts text from mixed documents, and writes one dump. Nothing is uploaded. The same core runs in three front ends: the CLI, the local mill (`pulp ui`, an axum server on `127.0.0.1`), and the browser mill (the core built to wasm32).

## Data flow

1. **Options** (`src/config.rs`): roots, format, tree mode, budgets, include and exclude globs, `Selection`. The CLI (`src/main.rs`) and the mill's API build them.
2. **Walk** (`src/walk.rs`, native only): one thread, depth first, each folder sorted by name, so budgets (`Budget`, `apply_budgets`, `cmp_path_order`) cut at the same file every run. Honors `.gitignore` through the `ignore` crate, the built-in excludes (`default_exclude_globs`), and `skip_identities` (the dump's own file). Files open through `open_beneath` (an `openat` walk with `O_NOFOLLOW` on unix), so a path swapped for a link after the scan is not followed.
3. **Manifest** (`src/manifest.rs`): the shared discovery result for scan, tree, and pack. Each entry has its `Kind` (`src/classify.rs`, from the name, then the bytes) and whether the mill ticks it by default.
4. **Pack** (`src/pack.rs`): reads each selected file as it is now, flags one changed since the scan, and extracts in parallel with rayon. Returns `Packed`: per-file text and `FileStatus`, plus `Stats`. `pack_manifest_cached` reuses the `ExtractCache` (`src/cache.rs`) of an earlier pack for files whose path, size, and modification time are unchanged.
5. **Extract** (`src/extract/`): one reader per kind, dispatched in `extract/mod.rs`. `check_signature` compares the bytes with the kind the name claims; `read_misnamed` reads a document that is really HTML or text as what it holds. `extract/isolate.rs` runs the kinds whose parsers can hang (PDF, Office, EPUB, RTF) in a child `pulp __extract` process with a timeout, and HTML whose nesting the pre-scan cannot vouch for.
6. **Render** (`src/render.rs`, `src/tree.rs`): the plain, Markdown, and XML layouts and the directory map. Names with control or bidi characters are escaped here.
7. **Output** (`src/main.rs`): stdout, or a temporary file beside `-o` renamed into place when complete; signals clean the temporary file up.

## Front ends

| Front end | Code | Notes |
| --- | --- | --- |
| CLI | `src/main.rs` | clap; the summary line and warnings go to stderr |
| Local mill | `src/ui.rs`, `src/store.rs`, `src/pick.rs`, `src/sample.rs` | axum on `127.0.0.1` (8747, or the next free port up to 8767), a per-process session token on every API call but `/api/health`, one pack job at a time; `store.rs` bounds the session's results and keeps the last pack's extraction cache; `pick.rs` opens the OS folder dialog |
| Browser mill | `crates/pulp-wasm/`, `site/mill/index.html`, `site/mill/worker.js` | The core without the `native` feature packs `MemoryFile`s (`pack_entries`). A pack worker per grant runs every pulp and extracts through a nested worker, so a parser that panics or hangs costs only its file |
| Shared interface | `web/` | `pulp.css`, `mill.css`, `mill.js`; `pulp ui` embeds them and `cargo xtask site` copies them into `site/` (ADR 0001) |

The local mill's API, all under `/api/` and all behind the session check (`require_session` in `src/ui.rs`): `scan`, `pack`, `tree`, `preview`, `render`, `cancel`, `progress`, `artifact/{id}`, `browse`, and `sample`. `/api/health` is open.

## Top level

| Path | Role |
| --- | --- |
| `src/` | The `pulp` crate, library and binary. The `native` feature (default) adds the walk, the CLI, and the local mill |
| `src/extract/` | Readers per kind, and the child-process isolation |
| `crates/pulp-wasm/` | wasm-bindgen bindings for the browser mill; its native tests run in `cargo test --workspace` |
| `web/` | The mill's interface, the sample project, the site's demo snapshot, and the browser tests (`web/test/`) |
| `site/` | The static website on Vercel (root `site`), with the browser mill at `/mill` and its committed package in `site/mill/pkg/` |
| `tests/`, `testdata/` | Integration tests: CLI, pack, formats, isolation, re-pulp |
| `vendor/pdf-extract/` | pdf-extract 0.12.1 with pulp's patches, wired in by `[patch.crates-io]` |
| `xtask/` | `cargo xtask`: host-aware build, the `site/` copy and its check, and `ui-test` (headless Chrome, plus a real `pulp ui` for `web/test/local.test.html`) |
| `docs/` | Human docs: usage, development, the browser mill |
| `.github/workflows/` | `ci.yml` (the gates and an MSRV job) and `main-from-dev.yml` (pull requests into `main` must come from `dev`) |
| `.agents/` | This material |

Release builds unwind on panic so a parser panic stays one file's error (ADR 0002).
