#!/usr/bin/env python3
"""Resolve one exact release version/tag for every distribution job."""

import os
from pathlib import Path
import re
import tomllib


def main():
    channel = os.environ.get("RELEASE_CHANNEL") or "stable"
    event = os.environ["GITHUB_EVENT_NAME"]
    if event == "push":
        channel = "stable"
    if channel not in ("stable", "nightly"):
        raise ValueError("unsupported release channel")
    root = Path(__file__).resolve().parent.parent
    workspace = tomllib.loads((root / "Cargo.toml").read_text(encoding="utf-8"))
    base = os.environ.get("RELEASE_VERSION") or workspace["workspace"]["package"]["version"]
    if event == "push":
        ref = os.environ["GITHUB_REF_NAME"]
        if not ref.startswith("v"):
            raise ValueError("release push must name an exact v-prefixed tag")
        base = ref[1:]
    if not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+(?:-[A-Za-z0-9.-]+)?", base):
        raise ValueError("release version must be a URL-safe semantic version")
    if channel == "nightly":
        sha = os.environ["GITHUB_SHA"]
        if not re.fullmatch(r"[0-9a-f]{40,64}", sha):
            raise ValueError("invalid release commit")
        version = f"{base}-nightly.{sha}"
        tag = f"nightly-{base}-{sha}"
    else:
        version = base
        tag = f"v{version}"
    with open(os.environ["GITHUB_OUTPUT"], "a", encoding="utf-8") as output:
        for key, value in (
            ("channel", channel), ("version", version), ("tag", tag),
            ("prerelease", str(channel == "nightly").lower()),
        ):
            output.write(f"{key}={value}\n")


if __name__ == "__main__":
    main()
