#!/usr/bin/env bash
# Run the steps of .github/workflows/ci.yml inside the Linux image, on a fresh
# copy of the checkout mounted read-only at /src. Every step runs even when an
# earlier one fails, so one pass reports all failures. run.sh starts it.
#
#   run-ci.sh          the ci job
#   run-ci.sh msrv     the msrv job: the rust-version Cargo.toml declares, warnings denied
set -uo pipefail

checkout=/home/runner/work/pulp/pulp
rsync -a --delete --exclude /target --exclude /.ruff_cache \
  --exclude /.agents/scratchpad --exclude /.agents/memory/sessions /src/ "$checkout/"
cd "$checkout" || exit 1
export CARGO_TARGET_DIR=/home/runner/target

if [[ "${1:-}" == "msrv" ]]; then
  version=$(sed -n 's/^rust-version = "\([0-9.]*\)"$/\1/p' Cargo.toml)
  echo "::: msrv toolchain $version"
  rustup toolchain install "$version" --profile minimal -t wasm32-unknown-unknown >/dev/null 2>&1 || exit 1
  export CARGO_TARGET_DIR=/home/runner/target/msrv RUSTFLAGS="-D warnings"
  names=("check" "check-no-default" "check-wasm32")
  commands=(
    "cargo +$version check --workspace --all-targets --locked"
    "cargo +$version check -p pulp --no-default-features --lib --locked"
    "cargo +$version check -p pulp-wasm --target wasm32-unknown-unknown --locked"
  )
else
  names=("rustfmt" "clippy" "clippy-no-default" "clippy-wasm32" "test" "site-check" "ui-test")
  commands=(
    "cargo fmt --all --check"
    "cargo clippy --workspace --all-targets --locked -- -D warnings"
    "cargo clippy -p pulp --no-default-features --lib --locked -- -D warnings"
    "cargo clippy -p pulp-wasm --target wasm32-unknown-unknown --locked -- -D warnings"
    "cargo test --workspace --locked"
    "cargo xtask site --check"
    "cargo xtask ui-test"
  )
fi

failed=()
for i in "${!names[@]}"; do
  name=${names[$i]}
  echo "::: step $name: ${commands[$i]}"
  start=$(date +%s)
  if bash -c "${commands[$i]}"; then
    echo "::: ok $name in $(( $(date +%s) - start ))s"
  else
    echo "::: FAILED $name in $(( $(date +%s) - start ))s"
    failed+=("$name")
  fi
done

echo "::: leftover processes:"
# ps, unlike pgrep, shows each process's parent, group, and age.
# shellcheck disable=SC2009
ps -eo pid,ppid,pgid,etimes,args | grep -Ei 'chrome|pulp' | grep -v grep || echo "  none"

if (( ${#failed[@]} )); then
  echo "::: failed steps: ${failed[*]}"
  exit 1
fi
echo "::: all steps passed"
