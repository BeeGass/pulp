# pulp.onlygass.dev

Static landing site for pulp.

## Deploy

- Host: Vercel (or any static host)
- Project root directory: `site`
- Production domain: `pulp.onlygass.dev`
- DNS: CNAME `pulp` → `cname.vercel-dns.com` (or the value Vercel shows) on the Squarespace zone for `onlygass.dev`

## Contents

| Path | Purpose |
| --- | --- |
| `/` | Product / install landing; its product shot is the real mill in demo mode (`/mill/demo.json`) |
| `/mill` | In-browser WASM mill |

The live axum mill (`pulp ui`) stays localhost-only. Do not point this site at a server-side packer.

## Browser mill

`/mill` is the in-browser WASM mill (`crates/pulp-wasm`). Rebuild with `wasm-pack` (see `docs/wasm-mill.md`) and commit updated `site/mill/pkg/*` artifacts.

`pulp.css`, `mill/mill.css`, `mill/mill.js`, `mill/sample.json`, `mill/demo.json`, and `fonts/` are copies of files in `web/`. Edit `web/` and run `cargo xtask site`; `cargo xtask test` fails if the copies drift.

The product shot's data, `web/sample-demo.json`, is a snapshot of pulp's real scan and pack of the sample project. When the sample or the dump format changes, regenerate it with `PULP_BLESS=1 cargo test --lib sample`, then run `cargo xtask site`.
