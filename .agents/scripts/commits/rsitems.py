"""Split a rustfmt-formatted `mod tests { ... }` block into named items.

usage (inside the repository):
  rsitems.py list <file|HEAD:path>     print the items of the file's test module
  rsitems.py split <file|HEAD:path>    print JSON {prefix, header, items:[{name,kind,text}], footer}

An item is a run of lines starting at the module's indentation (attributes,
doc comments, then `fn`, `async fn`, `use`, `const`, `struct`, ...) and ending
at the first line that is exactly the indentation plus `}` for braced items, or
at the first line ending in `;` for one-line items.

Use it to hand the tests a commit adds to the commit that adds the code they
test: split the old and new test modules, then write each commit's version of
the file from the prefix, the items it should hold, and the footer.
"""

import json
import re
import subprocess
import sys
from typing import TypedDict


class Item(TypedDict):
    name: str
    kind: str
    text: str


class Parts(TypedDict):
    prefix: str
    header: str
    items: list[Item]
    footer: str


def read(spec: str) -> str:
    if spec.startswith("HEAD:"):
        return subprocess.run(
            ["git", "show", spec], check=True, capture_output=True
        ).stdout.decode()
    with open(spec) as fh:
        return fh.read()


def split(text: str) -> Parts:
    lines = text.splitlines(keepends=True)
    start = None
    for i, line in enumerate(lines):
        if (
            line.rstrip("\n") == "mod tests {"
            and i > 0
            and lines[i - 1].strip() == "#[cfg(test)]"
        ):
            start = i - 1
            break
    if start is None:
        raise SystemExit("no test module")
    # the module ends at the last line that is exactly "}"
    end = max(i for i, line in enumerate(lines) if line.rstrip("\n") == "}")
    body = lines[start + 2 : end]
    items: list[Item] = []
    header: list[str] = []
    i = 0
    pending: list[str] = []
    while i < len(body):
        line = body[i]
        stripped = line.rstrip("\n")
        if not stripped.strip():
            if pending:
                pending.append(line)
            elif items:
                items[-1]["text"] += line
            else:
                header.append(line)
            i += 1
            continue
        if stripped.startswith(("    #[", "    ///", "    //")):
            pending.append(line)
            i += 1
            continue
        if not stripped.startswith("    ") or stripped.startswith("     "):
            raise SystemExit(f"unexpected line {i}: {stripped!r}")
        # an item's first code line
        chunk = [*pending, line]
        pending = []
        m = re.match(
            r"\s+(?:pub(?:\([a-z]+\))? )?(?:async )?(fn|const|static|struct|enum|use|impl|type|mod)\s+([A-Za-z_][A-Za-z0-9_]*)?",
            stripped,
        )
        kind = m.group(1) if m else "item"
        name = m.group(2) if m and m.group(2) else stripped.strip()
        if kind == "use":
            name = "use " + stripped.strip()
        one_line = stripped.endswith(";") and "{" not in stripped
        i += 1
        if not one_line:
            while i < len(body) and body[i].rstrip("\n") != "    }":
                chunk.append(body[i])
                i += 1
            if i < len(body):
                chunk.append(body[i])
                i += 1
        items.append({"name": name, "kind": kind, "text": "".join(chunk)})
    if pending:
        raise SystemExit("dangling attributes at end of module")
    return {
        "prefix": "".join(lines[: start + 2]),
        "header": "".join(header),
        "items": items,
        "footer": "".join(lines[end:]),
    }


def main() -> None:
    cmd, spec = sys.argv[1], sys.argv[2]
    parts = split(read(spec))
    if cmd == "list":
        for n, item in enumerate(parts["items"]):
            print(f"{n:3d} {item['kind']:<6} {item['name']}")
    elif cmd == "split":
        json.dump(parts, sys.stdout)
    else:
        raise SystemExit(__doc__)


if __name__ == "__main__":
    main()
