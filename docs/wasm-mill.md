# Browser WASM mill

Status: **shipped** at https://pulp.onlygass.dev/mill

The browser mill packs files you grant in the tab. Native `pulp ui` still walks the filesystem with gitignore and isolates hang-prone parsers.

## Capabilities

| Capability | Browser mill | Native mill (`pulp ui`) |
| --- | --- | --- |
| Files never leave the device | yes | yes |
| Choose folder / choose files | yes (pickers, or drop a folder or files) | OS file manager, or a typed path |
| Type an arbitrary filesystem path | no | yes |
| `.gitignore` discovery | no | yes |
| Default exclude globs (`*.key`, `target/**`, …) | yes, same core matcher | yes |
| Empty selection means nothing | yes | yes |
| Full dump vs capped preview | `artifact_as` draws the full dump from the stored result | `GET /api/artifact/{id}` |
| PDF / Office / zip extract | in-process WASM | child process + timeout |
| Damaged or encrypted files | flagged `unreadable`; the dump keeps a one-line note | same |
| Cooperative cancel | terminate worker | between files |
| Pack progress | files read into the tab | files extracted (`GET /api/progress`) |
| Format or directory-map change after a pulp | redraws the stored result (`render_result`) in the worker that holds it | redraws the stored result |
| File preview | extracted text (`preview_file`, stores nothing) | extracted text (`POST /api/preview`, one file from the last scan; runs beside a pack) |
| Results kept in memory | extracted files of the last 4 pulps per WASM instance; each pulp's worker is freed when a newer dump replaces it | extracted files of recent pulps, bounded by count and text size (`src/store.rs`) |
| Try a sample | `/mill/sample.json`, in memory | `POST /api/sample`, written to a temp folder |

## Layout

- Crate: `crates/pulp-wasm` (`wasm-bindgen`), artifacts in `site/mill/pkg/`.
- UI: `web/mill.js`, `web/mill.css`, and `web/pulp.css`, shared with `pulp ui`. `cargo xtask site` copies them (with the fonts, `web/sample.json`, and the site demo data) into `site/`; edit `web/`, not the copies.
- Browser shell and adapter: `site/mill/index.html` (scan on the main thread; pack, redraw, and preview in `site/mill/worker.js`). Asset paths are absolute (`/mill/...`) so `/mill` and `/mill/` both load.
- Shared extract/pack/render: `pulp` with `default-features = false`.

## Rebuild

```bash
wasm-pack build crates/pulp-wasm --target web --release --out-dir ../../site/mill/pkg
rm -f site/mill/pkg/.gitignore
```

On macOS, `.cargo/config.toml` links host build scripts with the Command Line Tools clang, and its `-isysroot` flags do not apply when the target is wasm32. If the build fails while linking a build script, point the SDK at the Command Line Tools copy for that command: `SDKROOT=/Library/Developer/CommandLineTools/SDKs/MacOSX.sdk wasm-pack build ...`.

After changing anything under `web/`, run `cargo xtask site` (or `cargo xtask site --check` in CI).

The result store, redraw, and preview logic in `crates/pulp-wasm` is plain Rust with native tests; `cargo xtask test` runs them with the rest of the workspace.

Serve `site/` locally. Vercel project **pulp-landing** uses Root Directory `site`.

## Non-goals

- Hosting the axum mill on a public hostname.
- Uploading folders to a server for packing.
