# Browser WASM mill

Status: **shipped** at https://pulp.onlygass.dev/mill

The browser mill packs files you grant in the tab. Native `pulp ui` still walks the filesystem with gitignore and isolates hang-prone parsers.

## Capabilities

| Capability | Browser mill | Native mill (`pulp ui`) |
| --- | --- | --- |
| Files never leave the device | yes | yes |
| Choose folder / choose files | yes (`<input>` / drag-drop) | OS file manager |
| Type an arbitrary filesystem path | no | yes |
| `.gitignore` discovery | no | yes |
| Default exclude globs (`*.key`, `target/**`, …) | yes, same core matcher | yes |
| Empty selection means nothing | yes | yes |
| Full dump vs capped preview | `artifact(result_id)` keeps the full dump | `GET /api/artifact/{id}` |
| PDF / Office / zip extract | in-process WASM | child process + timeout |
| Cooperative cancel | terminate worker | between files |

## Layout

- Crate: `crates/pulp-wasm` (`wasm-bindgen`), artifacts in `site/mill/pkg/`.
- UI: `site/mill/index.html`.
- Shared extract/pack/render: `pulp` with `default-features = false`.

## Rebuild

```bash
wasm-pack build crates/pulp-wasm --target web --release --out-dir ../../site/mill/pkg
rm -f site/mill/pkg/.gitignore
```

Serve `site/` locally. Vercel project **pulp-landing** uses Root Directory `site`.

## Non-goals

- Hosting the axum mill on a public hostname.
- Uploading folders to a server for packing.
