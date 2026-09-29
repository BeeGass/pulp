# 0001: Shared mill UI lives in web/ and is copied into site/

- Status: Accepted
- Date: 2026-09-29

## Context

The local mill (`pulp ui`, `web/index.html`) and the browser mill (`site/mill/index.html`) each carried their own copy of about a thousand lines of CSS and JavaScript. The copies drifted: different state styles, folder checkboxes and language chips in only one of them, and stale-dump tracking in only one of them.

The local mill embeds its files with `include_str!`. The site deploys `site/` as-is with no build step (Vercel root directory `site`), so it cannot reference files outside `site/`.

## Decision

- `web/pulp.css` (tokens and components), `web/mill.css` (mill layout), and `web/mill.js` (the whole mill UI, `mountMill(root, adapter)`) are the single source. Each surface keeps a thin HTML shell with an adapter for its packer: `/api/*` for `pulp ui`, WASM plus a worker for `/mill`, canned data for the landing page's product shot.
- `src/ui.rs` serves the three files at `/pulp.css`, `/mill.css`, and `/mill.js`.
- `cargo xtask site` copies them, the fonts, `web/sample.json`, and `web/sample-demo.json` into `site/`. An xtask test fails when a copy differs from its source.
- The sample project is one bundle, `web/sample.json`, so no nested `Cargo.toml` lands in the package tree. `web/sample-demo.json` is a snapshot test of pulp's real scan and pack of that sample; `PULP_BLESS=1` regenerates it.

## Consequences

- One edit changes both mills; `cargo xtask test` catches a forgotten sync.
- `site/` holds generated copies that must not be edited by hand.
- Both mills redraw a stored result in a new format or directory-map setting without extracting again: `pulp ui` through `/api/render`, the browser mill through `render_result` in the worker that holds the result.
