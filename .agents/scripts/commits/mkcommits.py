"""Build a chain of commits with git plumbing, never touching the worktree,
the real index, or any branch.

usage: mkcommits.py SPEC.json [PARENT]

Run it inside the repository. SPEC is a JSON list of commits:
{"subject", "body", "files": {path: source}}. A source is "worktree" (the
file as it is in the checkout now), "@/abs/path" (the bytes of a scratch
file), or null (delete the path). Files a commit does not name keep their
content from the previous commit. PARENT defaults to HEAD.

Subjects must fit in 50 characters. Body paragraphs are wrapped at 72;
paragraphs holding list items or indented lines are kept as written. The
author and committer come from GIT_AUTHOR_NAME, GIT_AUTHOR_EMAIL,
GIT_COMMITTER_NAME, and GIT_COMMITTER_EMAIL when set, else from git config.

Prints "n sha subject" per commit and writes the chain next to SPEC as
chain.txt. Nothing points at the new commits until you move a branch:
git update-ref refs/heads/BRANCH NEW_TIP OLD_TIP && git reset -q
"""

import json
import os
import subprocess
import sys
import tempfile
import textwrap
from typing import TypedDict


class Commit(TypedDict):
    subject: str
    body: str
    files: dict[str, str | None]


def find_repo() -> str:
    return subprocess.run(
        ["git", "rev-parse", "--show-toplevel"],
        check=True,
        capture_output=True,
        text=True,
    ).stdout.strip()


REPO = find_repo()


def git(
    *args: str, env: dict[str, str] | None = None, data: bytes | None = None
) -> bytes:
    return subprocess.run(
        ["git", "-C", REPO, *args],
        check=True,
        capture_output=True,
        env=dict(os.environ, **(env or {})),
        input=data,
    ).stdout


def wrap(body: str) -> str:
    out = []
    for para in body.strip("\n").split("\n\n"):
        lines = para.split("\n")
        if any(line.startswith(("  ", "- ", "* ")) for line in lines):
            out.append("\n".join(lines))
        else:
            out.append(
                textwrap.fill(
                    " ".join(line.strip() for line in lines),
                    72,
                    break_on_hyphens=False,
                    break_long_words=False,
                )
            )
    return "\n\n".join(out)


def source_bytes(path: str, source: str) -> bytes:
    if source == "worktree":
        with open(os.path.join(REPO, path), "rb") as fh:
            return fh.read()
    if source.startswith("@"):
        with open(source[1:], "rb") as fh:
            return fh.read()
    raise SystemExit(f"bad source for {path}: {source!r}")


def mode_of(path: str, source: str) -> str:
    if source == "worktree" and os.access(os.path.join(REPO, path), os.X_OK):
        return "100755"
    return "100644"


def main() -> None:
    spec_path = sys.argv[1]
    with open(spec_path) as fh:
        commits: list[Commit] = json.load(fh)
    parent = sys.argv[2] if len(sys.argv) > 2 else "HEAD"
    parent = git("rev-parse", "--verify", parent + "^{commit}").decode().strip()
    tmp = tempfile.mkdtemp(prefix="mkcommits-")
    env = {"GIT_INDEX_FILE": os.path.join(tmp, "index")}
    git("read-tree", parent, env=env)
    chain = []
    for n, commit in enumerate(commits, start=1):
        subject = commit["subject"]
        if len(subject) > 50:
            raise SystemExit("subject over 50 characters: " + subject)
        for path, source in commit["files"].items():
            if source is None:
                git("update-index", "--force-remove", path, env=env)
                continue
            blob = git("hash-object", "-w", "--stdin", data=source_bytes(path, source))
            git(
                "update-index",
                "--add",
                "--cacheinfo",
                f"{mode_of(path, source)},{blob.decode().strip()},{path}",
                env=env,
            )
        tree = git("write-tree", env=env).decode().strip()
        message = subject + "\n\n" + wrap(commit["body"]) + "\n"
        for line in message.split("\n")[2:]:
            if len(line) > 72:
                raise SystemExit("body line over 72 characters: " + line)
        parent = (
            git("commit-tree", tree, "-p", parent, "-F", "-", data=message.encode())
            .decode()
            .strip()
        )
        chain.append(f"{n} {parent} {subject}")
        print(chain[-1])
    with open(
        os.path.join(os.path.dirname(os.path.abspath(spec_path)), "chain.txt"), "w"
    ) as fh:
        fh.write("\n".join(chain) + "\n")


if __name__ == "__main__":
    main()
