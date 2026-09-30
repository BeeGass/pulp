# The browser mill's package

`site/mill/pkg/` is a committed build of `crates/pulp-wasm`, and nothing checks that it matches the source. Any change to code the wasm build compiles (most of `src/` outside the `native` modules, and `crates/pulp-wasm/`) leaves the live browser mill running the old engine until the package is rebuilt and committed.

After such a change:

1. Rebuild, per [docs/wasm-mill.md](../../docs/wasm-mill.md#rebuild). On macOS, prefix `wasm-pack` with `SDKROOT=/Library/Developer/CommandLineTools/SDKs/MacOSX.sdk`.
2. Delete `site/mill/pkg/.gitignore`. wasm-pack writes one holding `*`, which would drop the package from git.
3. Run `cargo test -p pulp-wasm`. `test_worker_build_stamp_names_the_packaged_wasm` fails and prints the `export const BUILD = '...'` line (the FNV-1a hash of `pulp_wasm_bg.wasm`) to put in `site/mill/worker.js`.
4. Commit the package and the `BUILD` line together, as their own `build(site):` commit after the source change.

That test is the only guard. The page hands its compiled module to any worker with the same `BUILD`, so a line left stale lets a worker from one deploy take a module its glue cannot link.
