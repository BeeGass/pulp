# Developing pulp

How to build pulp from source, run its checks, and change the mill and the website. For what pulp does and how to use it, see the [README](../README.md) and the [usage guide](usage.md).

## Prerequisites

- **Rust 1.88 or newer**, from [rustup](https://rustup.rs), with `rustfmt` and `clippy` (`rustup component add rustfmt clippy`).
- **Google Chrome or Chromium**, for the mill's browser tests.
- **The `wasm32-unknown-unknown` target** (`rustup target add wasm32-unknown-unknown`), for the browser mill's clippy check.
- **[wasm-pack](https://github.com/wasm-bindgen/wasm-pack)** (`cargo install wasm-pack`), only to rebuild the browser mill.
- **On macOS, the Command Line Tools** (`xcode-select --install`). Inside the repository, `.cargo/config.toml` links with their `clang` and SDK, so a build does not depend on Xcode's license being accepted.

```sh
git clone https://github.com/BeeGass/pulp
cd pulp
git checkout dev
cargo xtask doctor
```

## Repository layout

| Path | What it holds |
| --- | --- |
| `src/` | The `pulp` crate: the CLI (`main.rs`), the walk, classification, extraction, packing, rendering, and the local mill's server (`ui.rs`) |
| `src/extract/` | One reader per document kind (PDF, Office, EPUB, RTF, HTML, notebooks, archives, and so on), and the child-process isolation for the ones that can hang |
| `crates/pulp-wasm/` | The browser mill's WebAssembly bindings |
| `web/` | The mill's interface, shared by both mills, and its browser tests (`web/test/`) |
| `site/` | The static website, pulp.onlygass.dev, including the browser mill at `/mill` |
| `docs/` | Human documentation |
| `tests/`, `testdata/` | Integration tests and their fixtures |
| `vendor/pdf-extract/` | A patched copy of the `pdf-extract` crate |
| `xtask/` | `cargo xtask`, the build and test helper |
| `.github/workflows/` | CI |
| `.agents/` | Notes and scripts for coding agents working in this repository |

## `cargo xtask`

```sh
cargo xtask doctor            # OS, arch, detected jobs
cargo xtask build             # debug build of pulp
cargo xtask build --release   # release build tuned to this machine's CPU
cargo xtask test              # cargo test --workspace
cargo xtask clippy            # clippy on the workspace, warnings denied
cargo xtask fmt               # rustfmt; --check only reports
cargo xtask ui                # build and run pulp ui (--port N, --no-open)
cargo xtask site              # copy the shared mill UI from web/ into site/
cargo xtask ui-test           # the mill's browser tests; needs Chrome or Chromium
cargo xtask run -- --list .   # cargo run with pulp's arguments
```

`cargo xtask` sets the job count from available parallelism and, on macOS, prefers the Command Line Tools clang when it is installed. Named-lab overrides exist for a few machines; every other host is `unknown` and still gets a sensible default. `PULP_HOST` is only needed to deliberately pretend to be one of those machines.

On macOS and Linux, `cargo xtask build --release` adds `-C target-cpu=native`, so the binary is tuned to the machine that built it and may not run on an older CPU. Plain `cargo build --release` stays portable.

## Checks

CI runs these on every push and pull request (`.github/workflows/ci.yml`). Run them before opening a pull request:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo clippy -p pulp --no-default-features --lib --locked -- -D warnings
cargo clippy -p pulp-wasm --target wasm32-unknown-unknown --locked -- -D warnings
cargo test --workspace --locked
cargo xtask site --check
cargo xtask ui-test
```

On macOS, the `wasm32` clippy line needs the Command Line Tools SDK named for its build scripts: prefix it with `SDKROOT=/Library/Developer/CommandLineTools/SDKs/MacOSX.sdk`.

A second CI job checks the minimum supported Rust version, the `rust-version` in `Cargo.toml`, with warnings denied:

```sh
rustup toolchain install 1.88 --profile minimal -t wasm32-unknown-unknown
RUSTFLAGS="-D warnings" cargo +1.88 check --workspace --all-targets --locked
RUSTFLAGS="-D warnings" cargo +1.88 check -p pulp --no-default-features --lib --locked
RUSTFLAGS="-D warnings" cargo +1.88 check -p pulp-wasm --target wasm32-unknown-unknown --locked
```

`--locked` means `Cargo.lock` must already match the manifests, as `cargo install --locked` needs. After changing a dependency, commit the updated lockfile.

## The mill's interface

The mill's interface lives in `web/`: `pulp.css`, `mill.css`, `mill.js`, the fonts, the sample project (`sample.json`), and the website's product shot (`sample-demo.json`). `pulp ui` embeds them in the binary, with `web/index.html` as its page. `cargo xtask site` copies them into `site/` (`site/pulp.css`, `site/mill/mill.css`, `site/mill/mill.js`, `site/mill/sample.json`, `site/mill/demo.json`, and `site/fonts/`), and `cargo xtask test` fails if the copies drift, so edit `web/`, never the copies, and run `cargo xtask site` after each change.

Its browser tests live in `web/test/`. `cargo xtask ui-test` runs them, along with the landing page and the browser mill from `site/`, in headless Chrome or Chromium; `PULP_CHROME` picks the binary, and `--serve` serves the pages to your own browser instead. It also builds pulp and runs `web/test/local.test.html` against a real `pulp ui`, which it starts on a free port and stops afterwards.

The website's product shot is the real mill in demo mode, drawn from `web/sample-demo.json`, a snapshot of pulp's real scan and pack of the sample project. When the sample or the dump format changes, regenerate it with `PULP_BLESS=1 cargo test --lib sample`, then run `cargo xtask site`.

## The browser mill

The browser mill is `crates/pulp-wasm`, built with `wasm-pack` into `site/mill/pkg/`, whose files are committed so the static site can serve them. [wasm-mill.md](wasm-mill.md) covers its design and the rebuild steps, including the `BUILD` line in `site/mill/worker.js` that must name the new package.

## The website

`site/` is a static site with no build step. It is deployed on Vercel, though any static host would serve it:

- Project root directory: `site`
- Production domain: `pulp.onlygass.dev`
- DNS: CNAME `pulp` to `cname.vercel-dns.com` (or the value Vercel shows) on the Squarespace zone for `onlygass.dev`
- Headers, including the security headers: `site/vercel.json`

| Path | Purpose |
| --- | --- |
| `/` | Product and install page; its product shot is the real mill in demo mode (`/mill/demo.json`) |
| `/mill` | The in-browser WebAssembly mill |
| `/index.md`, `/mill.md`, `/llms.txt`, `/llms-full.txt` | Markdown mirrors and maps for agents and plain-text readers |

The local mill (`pulp ui`) stays on localhost. Do not point the site at a server-side packer.

## The vendored PDF reader

`vendor/pdf-extract` is `pdf-extract` 0.12.1 (MIT, by Jeff Muizelaar), wired in through `[patch.crates-io]` in `Cargo.toml`. Pulp's patches, each marked in the source, keep a hostile or unusual PDF from panicking or running away: a Type 3 font's missing widths, predefined Unicode CMaps, malformed operands, the `'` and `"` text operators, and bounds on nested form XObjects. Carry them forward when updating the crate.

## Branches and pull requests

Work lands on `dev`. `main` is the published line and moves only when `dev` is merged into it through a pull request; a check fails any pull request into `main` from another branch.

```sh
git checkout dev
git pull
# commits
git push origin dev
```

Then open a pull request from `dev` into `main`. Commit subjects follow [Conventional Commits](https://www.conventionalcommits.org) (`feat(mill): ...`, `fix(extract): ...`, `docs: ...`), as the history does.
