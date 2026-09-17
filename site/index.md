---
title: "pulp — LLM-ready dumps from local folders"
description: "Pulp grinds a local folder into one LLM-ready text file. Runs on your machine. Browser mill at /mill — files stay in the tab."
url: "https://pulp.onlygass.dev/"
markdown: "https://pulp.onlygass.dev/index.md"
author: "Bryan Gass"
---

# Pulp

Grind a local folder into one **LLM-ready dump**.

Pulp walks a tree in parallel, pulls text out of mixed documents, and writes a single file you can hand a model. Paper pulp. Juice. Disposable reading.

**Local first.** Files never leave the machine. The CLI mill binds localhost only. The [browser mill](https://pulp.onlygass.dev/mill) packs in WebAssembly inside your tab — still no upload.

## Install

```bash
cargo install --git https://github.com/BeeGass/pulp --locked
pulp -o dump.xml .
pulp ui
```

Requires Rust 1.85+.

## What it does

- **Mixed documents** — code, Markdown, HTML, JSON/CSV, PDF, Office, notebooks, archives
- **Formats you pick** — plain text, Markdown, or Claude-style XML (`-f` or `-o` extension)
- **Quiet by default** — skips `node_modules`, `target`, venvs, secrets, and other noisy trees
- **Local mill** — `pulp ui` at `127.0.0.1:8747`; nothing uploaded

## Browser mill

In-browser mill: [/mill](https://pulp.onlygass.dev/mill) · [mill.md](https://pulp.onlygass.dev/mill.md)

Same privacy story as localhost: packing runs in WASM in your tab. No server-side upload/packer. Local `pulp ui` still wins for gitignore walks and hang isolation.

## Agent maps

- [llms.txt](https://pulp.onlygass.dev/llms.txt)
- [llms-full.txt](https://pulp.onlygass.dev/llms-full.txt)
- [sitemap.xml](https://pulp.onlygass.dev/sitemap.xml)

## Source

https://github.com/BeeGass/pulp
