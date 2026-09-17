---
title: "pulp mill — in-browser LLM packer"
description: "In-browser Pulp mill: pack a local folder into one LLM-ready dump with WebAssembly. Files stay in the tab — nothing is uploaded."
url: "https://pulp.onlygass.dev/mill"
markdown: "https://pulp.onlygass.dev/mill.md"
author: "Bryan Gass"
---

# Pulp mill (browser)

The public mill UI at [pulp.onlygass.dev/mill](https://pulp.onlygass.dev/mill) packs a folder you choose in the browser into one LLM-ready dump.

## Privacy

- Packing runs **in-tab** with WebAssembly.
- **Nothing is uploaded** to a server-side packer.
- Files stay in the browser tab for the session.

## What it is for

- Quick dumps when you do not want to install the CLI
- Same product idea as `pulp ui` on localhost: select files, choose format (XML / Markdown / plain text), copy or download the dump

## Limits vs local CLI

Local `pulp ui` (`127.0.0.1`) still wins for:

- gitignore-aware walks
- hang isolation for stuck extractors
- full native extract path on your machine

## Related

- Product home: [pulp.onlygass.dev](https://pulp.onlygass.dev/) · [index.md](https://pulp.onlygass.dev/index.md)
- Install CLI: `cargo install --git https://github.com/BeeGass/pulp --locked`
- Source: https://github.com/BeeGass/pulp

This markdown mirror describes the mill for agents and plain-text readers. It does **not** include WASM binaries or worker assets.
