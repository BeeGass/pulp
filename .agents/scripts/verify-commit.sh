#!/usr/bin/env bash
# Run the CI job's gates on one commit in a scratch worktree, so the checkout
# being edited is never touched. Prints one line: the short sha and each
# gate's result. Each gate's log is kept in $VERIFY_DIR/logs/.
#
#   verify-commit.sh [REV]      default: HEAD
#
# VERIFY_DIR (default ${TMPDIR:-/tmp}/pulp-verify) holds the worktree, its
# target directory (several GB), and the logs. Keep it outside the checkout.
# When done: git worktree remove --force "$VERIFY_DIR/wt"
set -uo pipefail

repo=$(git rev-parse --show-toplevel) || exit 1
rev=$(git -C "$repo" rev-parse --verify --quiet "${1:-HEAD}^{commit}") || {
  echo "not a commit: ${1:-HEAD}" >&2
  exit 1
}
dir=${VERIFY_DIR:-${TMPDIR:-/tmp}/pulp-verify}
wt=$dir/wt
logs=$dir/logs
mkdir -p "$logs"
export CARGO_TARGET_DIR=$dir/target

if [[ ! -e "$wt/.git" ]]; then
  git -C "$repo" worktree add --quiet --detach "$wt" "$rev" || exit 1
fi
git -C "$wt" checkout --quiet --detach "$rev" || exit 1
cd "$wt" || exit 1

result=""
failed=0
# gate NAME COMMAND...: run COMMAND with its output in logs/NAME.log.
gate() {
  local name=$1
  shift
  if "$@" >"$logs/$name.log" 2>&1; then
    result="$result $name"
    return 0
  fi
  result="$result $name:FAIL"
  failed=1
  return 1
}

gate fmt cargo fmt --all --check
gate clippy cargo clippy --workspace --all-targets --locked -- -D warnings
gate clippy-lib cargo clippy -p pulp --no-default-features --lib --locked -- -D warnings
# On macOS the wasm32 build scripts link against the Command Line Tools SDK.
if [[ "$(uname -s)" == Darwin ]]; then
  gate clippy-wasm32 env SDKROOT=/Library/Developer/CommandLineTools/SDKs/MacOSX.sdk \
    cargo clippy -p pulp-wasm --target wasm32-unknown-unknown --locked -- -D warnings
else
  gate clippy-wasm32 cargo clippy -p pulp-wasm --target wasm32-unknown-unknown --locked -- -D warnings
fi
if gate test cargo test --workspace --locked; then
  passed=$(grep '^test result' "$logs/test.log" | awk '{s += $4} END {print s + 0}')
  result="$result($passed)"
fi
gate site cargo xtask site --check
if gate ui-test cargo xtask ui-test; then
  result="$result($(tail -1 "$logs/ui-test.log" | grep -o '[0-9]* passed'))"
fi

echo "${rev:0:9} |$result"
exit "$failed"
