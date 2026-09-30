# Architecture

Short map of this checkout, taken from [`README.md`](../README.md) and the top-level names. Human docs win if they disagree. Crate internals are not expanded here.

Pulp walks a local tree, extracts text from mixed documents, and writes one dump. The README says nothing is uploaded. `pulp ui` serves a loopback mill at `http://127.0.0.1:8747`. The CLI example is `pulp -o dump.xml .`.

## Top level

| Path | Role |
| --- | --- |
| `README.md` | Overview, mill, CLI, and `cargo xtask` commands |
| `docs/wasm-mill.md` | Only file under `docs/` |
| `xtask/` | `cargo xtask doctor`, `build`, `test`, `ui`, `site`, and `ui-test`, as the README writes them |
| `crates/` | Present. Not named as a directory in the README sections used here |
| `src/` | Present. Not named in those sections |
| `tests/` | Present. Exercised by `cargo xtask test` |
| `testdata/` | Present. Not named in those sections |
| `site/` | Static product site and browser mill. Shared UI files there are copies of `web/`; edit `web/` |
| `web/` | Mill UI source of truth (`pulp.css`, `mill.css`, `mill.js`, `sample.json`, `sample-demo.json`, fonts). `src/ui.rs` embeds it; `cargo xtask site` copies it into `site/`. Browser tests live in `web/test/`; `cargo xtask ui-test` runs them in headless Chrome |
| `vendor/` | Present. Not named in those sections |
| `Cargo.toml` | Workspace manifest |
| `Cargo.lock` | Lockfile. The install line uses `--locked` |
| `rustfmt.toml` | Present |
| `LICENSE` | License |
| `.github/` | CI: fmt, clippy, tests, the `site/` copy check, and the browser tests |

The README says release builds can use `cargo xtask build --release`, and that PDF, Office, EPUB, and RTF extractors run in a child `pulp` process.
