# Agent scripts

Scripts here are kept and rerun. Add one only when an agent will run it again as part of working in this repository. They are tools for working on pulp, not part of the product; the product's own tooling is `cargo xtask`.

One-off commands do not land here. Run those in the shell, or leave a note in the scratchpad if the investigation itself matters.

| Script | What it does |
| --- | --- |
| [`verify-commit.sh`](verify-commit.sh) `[REV]` | Runs every gate of CI's main job on one commit in a scratch worktree at `$VERIFY_DIR` (default `${TMPDIR:-/tmp}/pulp-verify`) and prints one line of results; logs stay in `$VERIFY_DIR/logs/`. Exits non-zero if a gate fails |
| [`linux-ci/run.sh`](linux-ci/run.sh) `[msrv]` | Builds the image in [`linux-ci/Dockerfile`](linux-ci/Dockerfile) (Ubuntu 24.04 x86_64, Rust stable, Chrome) and runs CI's steps, or the MSRV job, on a copy of the checkout through [`linux-ci/run-ci.sh`](linux-ci/run-ci.sh). Every step runs even after a failure. Setup notes are at the top of `run.sh` |
| [`build-mono-fonts.py`](build-mono-fonts.py) | Cuts the Pulp Mono webfont subsets in `web/fonts/` (referenced from `web/fonts/LICENSE.md`) |

`build-mono-fonts.py` needs fonttools and brotli; its docstring gives its usage.
