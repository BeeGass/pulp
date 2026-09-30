"""Cut the Pulp Mono webfont subsets used by the mill (web/fonts/plex-mono-*.woff2).

Source: IBM Plex Mono 2.3 TrueType from the IBM/plex GitHub release v6.3.0
(IBM-Plex-Mono/fonts/complete/ttf/IBMPlexMono-{Regular,Medium}.ttf).

The subset keeps every codepoint of the reference subset (the current file in
web/fonts), adds Box Drawing U+2500-257F plus a few symbols so dump trees render
in one font, and copies the reference's vertical metrics so layouts do not
shift. A subset is a Modified Version under the OFL, and IBM reserves the font
name "Plex", so the output is renamed "Pulp Mono".

Needs fonttools and brotli (pip install fonttools brotli).

Usage (from the repo root; bump the ?v= query in web/pulp.css afterwards and
run `cargo xtask site`):
  python .agents/scripts/build-mono-fonts.py IBMPlexMono-Regular.ttf \
      web/fonts/plex-mono-400.woff2 web/fonts/plex-mono-400.woff2 --weight 400 \
      --add U+2500-257F,U+2190,U+2192,U+21A9,U+2713
  python .agents/scripts/build-mono-fonts.py IBMPlexMono-Medium.ttf \
      web/fonts/plex-mono-500.woff2 web/fonts/plex-mono-500.woff2 --weight 500 \
      --add U+2500-257F,U+2190,U+2192,U+21A9,U+2713
"""

import argparse
import sys

from fontTools import subset
from fontTools.ttLib import TTFont

FEATURES = ["ccmp", "dnom", "frac", "numr", "mark"]
NAME_IDS = [0, 1, 2, 3, 4, 5, 6, 10, 14]
EXTRA_GLYPHS = ["a.alt01"]  # carried by the original web subset; kept for parity
USE_TYPO_METRICS = 1 << 7
NOTE = 'Subset of IBM Plex Mono 2.3, renamed because the OFL reserves the name "Plex" for unmodified versions.'
NAMES = {
    400: {
        1: "Pulp Mono",
        2: "Regular",
        3: "2.3;PULP;PulpMono-Regular",
        4: "Pulp Mono Regular",
        6: "PulpMono-Regular",
    },
    500: {
        1: "Pulp Mono Medium",
        2: "Regular",
        3: "2.3;PULP;PulpMono-Medium",
        4: "Pulp Mono Medium",
        6: "PulpMono-Medium",
    },
}


def parse_codepoints(spec: str) -> set[int]:
    out: set[int] = set()
    for part in spec.split(","):
        part = part.strip().upper().removeprefix("U+")
        if not part:
            continue
        if "-" in part:
            lo, hi = part.split("-")
            out.update(range(int(lo, 16), int(hi.removeprefix("U+"), 16) + 1))
        else:
            out.add(int(part, 16))
    return out


def make_options() -> subset.Options:
    opts = subset.Options()
    opts.layout_features = FEATURES
    opts.hinting = False
    opts.glyph_names = True
    opts.notdef_outline = True
    opts.name_IDs = NAME_IDS
    opts.name_legacy = False
    opts.name_languages = [0x409]
    opts.prune_codepage_ranges = False
    opts.drop_tables = [*opts.drop_tables, "meta"]
    opts.flavor = "woff2"
    return opts


def match_metrics(font: TTFont, ref: TTFont) -> None:
    """Copy the reference's vertical metrics and style bits."""
    src, dst = ref["OS/2"], font["OS/2"]
    dst.sTypoAscender = src.sTypoAscender
    dst.sTypoDescender = src.sTypoDescender
    dst.sTypoLineGap = src.sTypoLineGap
    dst.usWinAscent = src.usWinAscent
    dst.usWinDescent = src.usWinDescent
    dst.fsSelection = src.fsSelection
    assert dst.fsSelection & USE_TYPO_METRICS
    for attr in ("ascent", "descent", "lineGap"):
        setattr(font["hhea"], attr, getattr(ref["hhea"], attr))


def rename(font: TTFont, weight: int) -> None:
    table = font["name"]
    table.names = [rec for rec in table.names if rec.nameID in (0, 5, 14)]
    for name_id, value in NAMES[weight].items():
        table.setName(value, name_id, 3, 1, 0x409)
    table.setName(NOTE, 10, 3, 1, 0x409)


def build(source: str, reference: str, out: str, weight: int, extra: set[int]) -> None:
    ref = TTFont(reference)
    unicodes = set(ref.getBestCmap()) | extra
    opts = make_options()
    font = subset.load_font(source, opts, dontLoadGlyphNames=False)
    sub = subset.Subsetter(opts)
    sub.populate(unicodes=sorted(unicodes), glyphs=EXTRA_GLYPHS)
    sub.subset(font)
    match_metrics(font, ref)
    rename(font, weight)
    subset.save_font(font, out, opts)
    missing = sorted(cp for cp in extra if cp not in font.getBestCmap())
    if missing:
        print(
            "requested but absent from source:",
            " ".join(f"U+{cp:04X}" for cp in missing),
            file=sys.stderr,
        )


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("source")
    ap.add_argument("reference")
    ap.add_argument("out")
    ap.add_argument("--weight", type=int, choices=sorted(NAMES), required=True)
    ap.add_argument("--add", default="")
    args = ap.parse_args()
    build(
        args.source, args.reference, args.out, args.weight, parse_codepoints(args.add)
    )


if __name__ == "__main__":
    main()
