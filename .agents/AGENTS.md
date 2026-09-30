# Agent contract — pulp

Operating contract for agents editing this checkout. Read this file before changing code.

## Do not copy the human handbook

Do not copy human handbooks into `.agents/`. Human docs stay in [`README.md`](../README.md) and [`docs/`](../docs/). There is no CONTRIBUTING file; [`docs/development.md`](../docs/development.md) plays that role. If a human document and a note under `.agents/` disagree, the human document wins.

## Where to look

| Path | What it is |
| --- | --- |
| [`README.md`](../README.md) | The landing page: install, quick start, options, citation, license |
| [`docs/usage.md`](../docs/usage.md) | The full user guide: CLI, output, what goes in, the local mill, the library, troubleshooting |
| [`docs/development.md`](../docs/development.md) | Building from source, `cargo xtask`, the CI checks, the mill's interface, the website, branches |
| [`docs/wasm-mill.md`](../docs/wasm-mill.md) | The browser mill: design, its differences from `pulp ui`, rebuilding its package |
| [`src/`](../src/) | The `pulp` crate: CLI, walk, extract, pack, render, and the local mill |
| [`crates/pulp-wasm/`](../crates/pulp-wasm/) | The browser mill's WebAssembly bindings |
| [`web/`](../web/) | Mill UI source of truth, embedded by `pulp ui` and copied into `site/` by `cargo xtask site`; browser tests in `web/test/` |
| [`site/`](../site/) | The static website and the browser mill at `/mill` |
| [`tests/`](../tests/), [`testdata/`](../testdata/) | Integration tests and fixtures |
| [`vendor/pdf-extract/`](../vendor/pdf-extract/) | Patched pdf-extract |
| [`xtask/`](../xtask/) | `cargo xtask` |
| [`CITATION.cff`](../CITATION.cff), [`LICENSE`](../LICENSE) | Citation metadata and the MIT license |

[`ARCHITECTURE.md`](ARCHITECTURE.md) maps the code and its data flow.

Pulp grinds a local folder into one LLM-ready dump. Files never leave the machine, and the local mill binds `127.0.0.1` only (default port 8747). Product site: pulp.onlygass.dev.

## Commands

Build, test, and check commands are in [`docs/development.md`](../docs/development.md); CI runs the list under its "Checks" heading, and every change must pass all of it. Do not invent a separate lint command. [`memory/verification.md`](memory/verification.md) covers what trips agents running them, and two scripts run them for you:

```bash
.agents/scripts/verify-commit.sh [REV]   # every CI gate on one commit, in a scratch worktree
.agents/scripts/linux-ci/run.sh [msrv]   # CI's steps in an Ubuntu x86_64 container
```

Commits, pushes, and merges follow [`memory/commits-and-merging.md`](memory/commits-and-merging.md).

## Where agent material goes

- Durable facts go in [`MEMORY.md`](MEMORY.md) and notes under [`memory/`](memory/). Session dumps go in [`memory/sessions/`](memory/sessions/).
- Codebase-specific agent documentation goes in [`docs/`](docs/). That directory is not the human [`docs/`](../docs/) tree.
- Decisions are indexed from [`ADR.md`](ADR.md). One decision is one file, `.agents/adr/NNNN-short-title.md`. Do not invent past decisions.
- System shape goes in [`ARCHITECTURE.md`](ARCHITECTURE.md).
- Durable scripts that are kept and rerun go in [`.agents/scripts/`](scripts/). One-off commands do not land there.
- Reviews, audits, and other working files go in [`scratchpad/`](scratchpad/).

Product code stays in the source trees above. Do not relocate it into `.agents/`.
