"""Split a file's HEAD -> worktree change into hunks and rebuild partial versions.

usage (inside the repository; paths are relative to its root):
  hunks.py list <path>                 print hunks with ids and a short preview
  hunks.py show <path> <id>            print one hunk in full
  hunks.py build <path> <plan.json> <upto>
                                       print the file with every piece whose commit
                                       index is <= upto applied (plan maps piece ids
                                       to commit indexes; unlisted pieces are errors)
  hunks.py check <path> <plan.json>    verify the plan covers every piece and that
                                       applying all of them reproduces the worktree

A piece is a whole hunk ("7") or, for a pure insertion, a slice of its added
lines ("7:0-40" = added lines 0..40, half-open). Slices of one hunk are applied
in their original order, so the final file comes out identical.

A plan value is a commit index, or a list of [index, alt] stages sorted by
index, where alt names a file of replacement lines for that stage (or null for
the real lines). Stages let an early commit carry a simpler version of a piece
that a later commit replaces.

Feed `build` output to mkcommits.py as "@/path/to/built-file" sources.
"""

import json
import re
import subprocess
import sys
from typing import TypedDict

PlanValue = int | list[list[int | str | None]]


class Hunk(TypedDict):
    old_start: int
    old_count: int
    removed: list[str]
    added: list[str]


def find_repo() -> str:
    return subprocess.run(
        ["git", "rev-parse", "--show-toplevel"],
        check=True,
        capture_output=True,
        text=True,
    ).stdout.strip()


REPO = find_repo()


def git(*args: str) -> bytes:
    return subprocess.run(
        ["git", "-C", REPO, *args], check=True, capture_output=True
    ).stdout


def head_lines(path: str) -> list[str]:
    try:
        text = git("show", "HEAD:" + path).decode()
    except subprocess.CalledProcessError:
        text = ""
    return text.splitlines(keepends=True)


def parse(path: str) -> list[Hunk]:
    diff = git("diff", "-U0", "--no-color", "HEAD", "--", path).decode()
    hunks: list[Hunk] = []
    cur: Hunk | None = None
    for line in diff.splitlines(keepends=True):
        m = re.match(r"@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@", line)
        if m:
            cur = {
                "old_start": int(m.group(1)),
                "old_count": int(m.group(2)) if m.group(2) is not None else 1,
                "removed": [],
                "added": [],
            }
            hunks.append(cur)
            continue
        if cur is None:
            continue
        if line.startswith("-") and not line.startswith("---"):
            cur["removed"].append(line[1:])
        elif line.startswith("+") and not line.startswith("+++"):
            cur["added"].append(line[1:])
        elif line.startswith("\\"):
            # "\ No newline at end of file" applies to the previous line
            target = cur["added"] if cur["added"] else cur["removed"]
            if target and target[-1].endswith("\n"):
                target[-1] = target[-1][:-1]
    return hunks


def pieces_of(
    plan_keys: list[str], hunks: list[Hunk]
) -> dict[int, list[tuple[int, int, str]]]:
    """Map hunk index -> list of (lo, hi, key) slices in added-line order."""
    by_hunk: dict[int, list[tuple[int, int, str]]] = {}
    for key in plan_keys:
        if ":" in key:
            h, rng = key.split(":")
            lo, hi = rng.split("-")
            by_hunk.setdefault(int(h), []).append((int(lo), int(hi), key))
        else:
            by_hunk.setdefault(int(key), []).append(
                (0, len(hunks[int(key)]["added"]), key)
            )
    for slices in by_hunk.values():
        slices.sort()
    return by_hunk


def stage_for(value: PlanValue, upto: int) -> str | None:
    """The content choice for a piece at commit `upto`.

    Returns None when the piece is not applied yet, "real" for the real lines,
    or the path of a file of replacement lines.
    """
    if isinstance(value, int):
        return "real" if value <= upto else None
    chosen = None
    for index, alt in value:
        if isinstance(index, int) and index <= upto:
            chosen = alt if isinstance(alt, str) and alt else "real"
    return chosen


def build(path: str, plan: dict[str, PlanValue], upto: int) -> str:
    hunks = parse(path)
    orig = head_lines(path)
    by_hunk = pieces_of(list(plan), hunks)
    out: list[str] = []
    pos = 0
    for i, h in enumerate(hunks):
        slices = by_hunk.get(i, [])
        chosen: list[list[str]] = []
        for lo, hi, key in slices:
            stage = stage_for(plan[key], upto)
            if stage is None:
                continue
            if stage == "real":
                chosen.append(h["added"][lo:hi])
            else:
                with open(stage) as fh:
                    chosen.append(fh.read().splitlines(keepends=True))
        if not chosen:
            continue
        if h["old_count"] > 0:
            if len(slices) != 1 or slices[0][0] != 0:
                raise SystemExit(f"hunk {i} replaces lines and cannot be sliced")
            start = h["old_start"] - 1
        else:
            start = h["old_start"]
        out.extend(orig[pos:start])
        for lines in chosen:
            out.extend(lines)
        pos = start + h["old_count"]
    out.extend(orig[pos:])
    return "".join(out)


def check(path: str, plan: dict[str, PlanValue]) -> list[str]:
    hunks = parse(path)
    by_hunk = pieces_of(list(plan), hunks)
    problems = []
    for i, h in enumerate(hunks):
        slices = by_hunk.get(i)
        if not slices:
            problems.append(f"hunk {i} unassigned")
            continue
        covered = 0
        for lo, hi, _key in slices:
            if lo != covered:
                problems.append(f"hunk {i} gap at {covered}")
            covered = hi
        if covered != len(h["added"]) and h["added"]:
            problems.append(
                f"hunk {i} covers {covered} of {len(h['added'])} added lines"
            )
    with open(REPO + "/" + path) as fh:
        if fh.read() != build(path, plan, 10**9):
            problems.append("full build differs from the worktree")
    return problems


def load_plan(plan_path: str) -> dict[str, PlanValue]:
    with open(plan_path) as fh:
        plan: dict[str, PlanValue] = json.load(fh)
    return plan


def main() -> None:
    cmd, path = sys.argv[1], sys.argv[2]
    if cmd == "list":
        for i, h in enumerate(parse(path)):
            first = (h["added"] or h["removed"] or [""])[0].strip()[:100]
            print(
                f"{i:3d}  -{len(h['removed'])}+{len(h['added'])}  @{h['old_start']}  {first}"
            )
    elif cmd == "show":
        h = parse(path)[int(sys.argv[3])]
        print(f"@@ old {h['old_start']} count {h['old_count']}")
        for line in h["removed"]:
            print("-" + line, end="" if line.endswith("\n") else "\n")
        for n, line in enumerate(h["added"]):
            print(f"+{n:4d} " + line, end="" if line.endswith("\n") else "\n")
    elif cmd == "build":
        sys.stdout.write(build(path, load_plan(sys.argv[3]), int(sys.argv[4])))
    elif cmd == "check":
        problems = check(path, load_plan(sys.argv[3]))
        print("\n".join(problems) or "ok")
        sys.exit(1 if problems else 0)
    else:
        raise SystemExit(__doc__)


if __name__ == "__main__":
    main()
