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
| `/` | Product / install landing |
| `/mill/` | Placeholder for the in-browser WASM mill |

The live axum mill (`pulp ui`) stays localhost-only. Do not point this site at a server-side packer.
