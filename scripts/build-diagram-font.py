#!/usr/bin/env python3
"""Regenerate the checked-in diagram font; never run during ordinary builds.

Requires fonttools 4.60.1. Pass the pinned NotoEmoji variable font described in
packages/fonts/assets/diagram/README.md. No network access is performed here.
"""
import argparse
import hashlib
from pathlib import Path
import tempfile

from fontTools import subset
from fontTools.merge import Merger
from fontTools.ttLib import TTFont
from fontTools.ttLib.scaleUpem import scale_upem
from fontTools.varLib.instancer import instantiateVariableFont

ROOT = Path(__file__).resolve().parents[1]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("noto_emoji", type=Path)
    args = parser.parse_args()
    if hashlib.sha256(args.noto_emoji.read_bytes()).hexdigest() != "de6c18832938afc99caf132b39d6a30a19bac7f2e812e28db2535b4608d27551":
        parser.error("Noto Emoji input does not match the pinned source")
    base = ROOT / "packages/fonts/assets/nerd-fonts/JetBrainsMonoNerdFont-Regular.ttf"
    output = ROOT / "packages/fonts/assets/diagram/BmuxDiagram-Regular.ttf"
    output.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory() as scratch:
        scratch = Path(scratch)
        emoji = instantiateVariableFont(TTFont(args.noto_emoji), {"wght": 400}, inplace=True)
        # Single-codepoint symbols/emoji are deterministic. Complex ZWJ sequences
        # and full international-script coverage are not claimed by this font.
        emoji_codes = set(emoji.getBestCmap())
        latin = TTFont(base)
        scale_upem(emoji, latin["head"].unitsPerEm)
        # Keep technical-label alphabets/punctuation, discard large private-use
        # Nerd patches. Prefer Noto for its symbol/emoji coverage.
        codes = {c for c in latin.getBestCmap() if c < 0x3000 and (c < 0x2000 or c not in emoji_codes)}
        emoji_codes -= codes
        for font, chars, name in [(latin, codes, "base"), (emoji, emoji_codes, "emoji")]:
            options = subset.Options()
            options.recalc_timestamp = False
            options.layout_features = []  # Match the guest's advance-based metrics.
            tool = subset.Subsetter(options=options)
            tool.populate(unicodes=chars)
            tool.subset(font)
            for tag in ["vhea", "vmtx"]:
                if tag in font:
                    del font[tag]
            if "OS/2" in font:
                font["OS/2"].version = 4
                for field in ["sxHeight", "sCapHeight", "usDefaultChar", "usBreakChar", "usMaxContext"]:
                    if not hasattr(font["OS/2"], field):
                        setattr(font["OS/2"], field, 0)
            font.save(scratch / f"{name}.ttf")
        # Normalize hinting metadata that fontTools cannot merge across fonts
        # with and without native TrueType bytecode. Outlines remain unchanged.
        for name in ["base", "emoji"]:
            font = TTFont(scratch / f"{name}.ttf")
            for tag in ["maxp"]:
                table = font[tag]
                for field in ["maxZones", "maxTwilightPoints", "maxStorage", "maxFunctionDefs", "maxInstructionDefs", "maxStackElements", "maxSizeOfInstructions"]:
                    if not hasattr(table, field):
                        setattr(table, field, 0)
            font.save(scratch / f"{name}.ttf")
        merged = Merger().merge([str(scratch / "base.ttf"), str(scratch / "emoji.ttf")])
        names = {1: "BMUX Diagram", 2: "Regular", 3: "BMUX Diagram Regular 1", 4: "BMUX Diagram Regular", 6: "BmuxDiagram-Regular", 16: "BMUX Diagram", 17: "Regular"}
        for record in merged["name"].names:
            if record.nameID in names:
                record.string = names[record.nameID].encode(record.getEncoding())
        merged["head"].created = merged["head"].modified = 2082844800
        merged.recalcTimestamp = False
        merged.save(output)
    print(output, hashlib.sha256(output.read_bytes()).hexdigest())


if __name__ == "__main__":
    main()
