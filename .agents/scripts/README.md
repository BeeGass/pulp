# Agent scripts

Scripts here are kept and rerun. Add one only when an agent will run it again as part of working in this repository. They are tools for working on pulp, not part of the product; the product's own tooling is `cargo xtask`.

One-off commands do not land here. Run those in the shell, or leave a note in the scratchpad if the investigation itself matters.

| Script | What it does |
| --- | --- |
| [`verify-commit.sh`](verify-commit.sh) `[REV]` | Runs every gate of CI's main job on one commit in a scratch worktree at `$VERIFY_DIR` (default `${TMPDIR:-/tmp}/pulp-verify`) and prints one line of results; logs stay in `$VERIFY_DIR/logs/`. Exits non-zero if a gate fails |
| [`linux-ci/run.sh`](linux-ci/run.sh) `[msrv]` | Builds the image in [`linux-ci/Dockerfile`](linux-ci/Dockerfile) (Ubuntu 24.04 x86_64, Rust stable, Chrome) and runs CI's steps, or the MSRV job, on a copy of the checkout through [`linux-ci/run-ci.sh`](linux-ci/run-ci.sh). Every step runs even after a failure. Setup notes are at the top of `run.sh` |
| [`commits/mkcommits.py`](commits/mkcommits.py) `SPEC.json [PARENT]` | Builds a chain of commits with git plumbing from a JSON spec, without touching the working tree, the index, or any branch. Enforces the subject and body limits |
| [`commits/hunks.py`](commits/hunks.py) | Lists a file's uncommitted hunks and rebuilds the file with only some of them, so one file's changes can be split across commits |
| [`commits/rsitems.py`](commits/rsitems.py) | Splits a Rust file's `mod tests` into items, so each commit can carry the tests for the code it adds |
| [`build-mono-fonts.py`](build-mono-fonts.py) | Cuts the Pulp Mono webfont subsets in `web/fonts/` (referenced from `web/fonts/LICENSE.md`) |

The Python scripts need Python 3.11 or newer and nothing outside the standard library, except `build-mono-fonts.py` (fonttools and brotli). Each prints its usage in its docstring.

A typical split of finished work into commits: write a spec naming each commit's files (`"worktree"`, a scratch copy built with `hunks.py build`, or `null` to delete), run `mkcommits.py` from inside the repository with the identity in [`../memory/commits-and-merging.md`](../memory/commits-and-merging.md) exported, check each commit in `chain.txt` with `verify-commit.sh`, then move the branch with `git update-ref`.
