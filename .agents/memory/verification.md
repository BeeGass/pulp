# Verification

The gate list is in [docs/development.md](../../docs/development.md#checks). What trips agents running it:

- **`cargo xtask clippy` is not CI's clippy.** It skips `--locked` and the two extra runs: `-p pulp --no-default-features --lib`, and `-p pulp-wasm --target wasm32-unknown-unknown`. Run all three.
- **The wasm32 clippy run on macOS** fails linking build scripts unless `SDKROOT=/Library/Developer/CommandLineTools/SDKs/MacOSX.sdk` is set for that command. `.cargo/config.toml`'s `-isysroot` flags only apply to Darwin targets.
- **`cargo xtask ui-test`** needs Chrome or Chromium. It builds pulp, starts its own `pulp ui` on a free port, and stops it. Leave any other `pulp ui` alone; the user may be running one.
- **Snapshot tests.** `sample::tests::test_sample_demo_snapshot_is_current` fails after any change to the sample project or the dump format: re-bless with `PULP_BLESS=1 cargo test --lib sample`, then `cargo xtask site`. `test_worker_build_stamp_names_the_packaged_wasm` fails after a browser mill rebuild until `site/mill/worker.js` names the new package (see [browser-mill-package](browser-mill-package.md)).
- **`site/` copies.** `cargo xtask site --check` (and `cargo test`) fail when a `web/` file changed without `cargo xtask site`.
- **GitHub's checkout path.** CI checks out at `/home/runner/work/pulp/pulp`, where the checkout and its parent share a name. A root-label test once failed only there. The Linux container script copies the checkout to the same path.
- **Disk.** One target directory for the workspace, wasm32 included, is about 4 GB. Point parallel builds at separate `CARGO_TARGET_DIR`s and delete them when done.

## Scripts

- [`scripts/verify-commit.sh`](../scripts/verify-commit.sh) `[REV]` runs every CI gate on one commit in a scratch worktree (outside the checkout, `$VERIFY_DIR`), so a chain of commits can be checked one commit at a time while the working tree stays untouched.
- [`scripts/linux-ci/run.sh`](../scripts/linux-ci/run.sh) runs CI's steps (or `msrv`) in an Ubuntu 24.04 x86_64 container with Chrome, on a copy of the checkout. Use it before pushing anything that touches the harness, paths, or platform code; macOS alone has missed Linux-only failures.
