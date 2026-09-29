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
- The same interface as `pulp ui` on localhost: choose a folder or files (or drop them on the page), tick what goes in, choose readable text or source and XML / Markdown / plain text, then copy or download the dump
- **Try a sample** loads a small built-in project so you can see a dump before choosing your own files
- Selecting a file name previews the text pulp extracts from it; files that could not be extracted are flagged in the tree with the reason

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
