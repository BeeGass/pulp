#!/usr/bin/env bash
# Run CI's steps in a Linux container close to GitHub's ubuntu-24.04 runner,
# on a copy of this checkout (uncommitted changes included).
#
#   run.sh          the ci job: fmt, the three clippy runs, tests, site check, ui-test
#   run.sh msrv     the msrv job
#
# Needs a Docker engine that runs linux/amd64 images. On an Apple Silicon Mac,
# a colima profile with Rosetta works; the default 2 GB VM is too small:
#   colima start pulp-ci --cpu 6 --memory 8 --disk 60 --vm-type vz --vz-rosetta
# Build caches live in the volumes pulp-ci-target, pulp-ci-registry, and
# pulp-ci-git. To reclaim the space afterwards:
#   colima delete pulp-ci --data --force && docker context use colima
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
repo=$(git -C "$here" rev-parse --show-toplevel)
image=pulp-ci:ubuntu24-amd64

docker build --quiet --platform linux/amd64 -t "$image" "$here" >/dev/null
exec docker run --rm --platform linux/amd64 \
  -v "$repo":/src:ro \
  -v pulp-ci-target:/home/runner/target \
  -v pulp-ci-registry:/home/runner/.cargo/registry \
  -v pulp-ci-git:/home/runner/.cargo/git \
  "$image" run-ci.sh "$@"
