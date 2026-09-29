# 0002: Native release builds unwind on panic

- Status: Accepted
- Date: 2026-09-29

## Context

`[profile.release]` set `panic = "abort"`. Three parts of pulp contain a panic and rely on it unwinding:

- `extract_contained` in `src/pack.rs` wraps every in-process extractor in `catch_unwind`, so a PDF or Office parser that panics on a hostile file becomes an error note on that one file.
- `pulp ui` runs packs, previews, and the folder picker on blocking tasks and answers a task that panicked with a generic 500 (`src/ui.rs`).
- The sample project removes its folder after a write that panicked (`src/sample.rs`).

Under `panic = "abort"` each of these was a process abort instead: one malformed document ended a CLI run, or took down the mill with every stored result. Tests build with the test profile, which unwinds, so they passed while release builds behaved differently.

## Decision

Native release builds use Rust's default, `panic = "unwind"`; the line is gone from `[profile.release]`. The browser build does not change: `wasm32-unknown-unknown` always aborts, and `crates/pulp-wasm` turns that trap into a named JS error so the page retires the worker's instance.

## Consequences

- A panicking extractor costs one file in release builds, as it already did in tests and debug builds.
- Release binaries carry unwinding code and are slightly larger.
- Code that must survive a panic still needs a boundary, `catch_unwind` or a task; a panic while a lock is held poisons that lock, which callers must handle.
