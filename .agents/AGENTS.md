# Agent contract — pulp

Operating contract for agents editing this checkout. Read this file before changing code.

## Do not copy the human handbook

Do not copy human handbooks into `.agents/`. Human docs stay in [`README.md`](../README.md) and [`docs/`](../docs/). There is no CONTRIBUTING file. If a human document and a note under `.agents/` disagree, the human document wins.

## Where to look

| Path | What it is |
| --- | --- |
| [`README.md`](../README.md) | What pulp is, the local mill, and the CLI |
| [`docs/wasm-mill.md`](../docs/wasm-mill.md) | The only file under `docs/` |
| [`crates/`](../crates/) | Present. The README sections used here do not name crate directories |
| [`src/`](../src/) | Present. Not named in those README sections |
| [`tests/`](../tests/) | Present. The documented test command is `cargo xtask test` |
| [`testdata/`](../testdata/) | Present. Not named in those README sections |
| [`xtask/`](../xtask/) | The README's from-source commands are `cargo xtask` |
| [`site/`](../site/) | Present. The README links pulp.onlygass.dev |
| [`web/`](../web/) | Mill UI source of truth, embedded by `pulp ui` and copied into `site/` by `cargo xtask site` |
| [`vendor/`](../vendor/) | Present. Not named in those README sections |
| [`Cargo.toml`](../Cargo.toml) | Workspace manifest |
| [`LICENSE`](../LICENSE) | License |

Pulp grinds a local folder into one LLM-ready dump. The README says files never leave the machine, and the mill binds `127.0.0.1` only. Default mill port is 8747. Product site: pulp.onlygass.dev.

Commands the README documents:

```bash
cargo install --git https://github.com/BeeGass/pulp --locked
pulp ui
pulp -o dump.xml .
cargo xtask doctor
cargo xtask build
cargo xtask test
cargo xtask ui
cargo xtask site
cargo xtask ui-test
```

It also documents `pulp ui --port 9000 --no-open` and `cargo xtask ui-test --serve`. Rust 1.85 or newer is required, as written there. Do not invent a separate lint command.

## Where agent material goes

- Durable facts go in [`MEMORY.md`](MEMORY.md) and notes under [`memory/`](memory/). Session dumps go in [`memory/sessions/`](memory/sessions/).
- Codebase-specific agent documentation goes in [`docs/`](docs/). That directory is not the human [`docs/`](../docs/) tree.
- Decisions are indexed from [`ADR.md`](ADR.md). One decision is one file, `.agents/adr/NNNN-short-title.md`. Do not invent past decisions.
- System shape goes in [`ARCHITECTURE.md`](ARCHITECTURE.md).
- Durable scripts that are kept and rerun go in [`.agents/scripts/`](scripts/). One-off commands do not land there.
- Reviews, audits, and other working files go in [`scratchpad/`](scratchpad/).

Product code stays in the source trees above. Do not relocate it into `.agents/`.
