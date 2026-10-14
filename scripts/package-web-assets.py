#!/usr/bin/env python3
"""Package exact web assets for an immutable GitHub release (explicit web build only).

The renderer identity comes from A's shared Rust provenance implementation, not
an independent approximation. Invoke through scripts/dev.sh; this command may
compile the identity example but is never part of a native product build.
"""

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile
import tomllib

NAMES = (
    "index.html", "bootstrap.js", "style.css", "bmux_web_renderer.js",
    "bmux_web_renderer_bg.wasm",
)
MAX_FILE = 8 * 1024 * 1024
MAX_TOTAL = 24 * 1024 * 1024


def sync_directory(path):
    descriptor = os.open(path, os.O_RDONLY | os.O_DIRECTORY)
    try:
        os.fsync(descriptor)
    finally:
        os.close(descriptor)


def durable_file(path, data):
    with path.open("xb") as stream:
        stream.write(data)
        stream.flush()
        os.fsync(stream.fileno())


def package(root, assets, output):
    if not os.environ.get("BMUX_NIX_TOOLCHAIN"):
        raise ValueError("run inside nix develop or bash scripts/dev.sh")
    tag = os.environ.get("BMUX_WEB_RELEASE_TAG", "")
    if not re.fullmatch(r"[A-Za-z0-9._-]+", tag) or tag == "latest":
        raise ValueError("BMUX_WEB_RELEASE_TAG must name the exact immutable release")
    identity = subprocess.check_output(
        ["cargo", "run", "--locked", "--quiet", "-p", "bmux_web_host",
         "--example", "renderer-identity", "--", str(root)],
        cwd=root, text=True,
    ).strip()
    if not re.fullmatch(r"[0-9a-f]{64}", identity):
        raise ValueError("shared Rust provenance helper returned an invalid identity")
    workspace = tomllib.loads((root / "Cargo.toml").read_text(encoding="utf-8"))
    renderer = tomllib.loads((root / "packages/web-renderer/Cargo.toml").read_text(encoding="utf-8"))
    version = workspace["workspace"]["package"]["version"]
    bindgen = renderer["target"]['cfg(target_arch = "wasm32")']["dependencies"]["wasm-bindgen"]["version"].removeprefix("=")
    if output.exists():
        raise ValueError("output must be a new directory, never overwrite a partial release")
    output.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix=".web-release-", dir=output.parent) as temporary:
        staging = Path(temporary) / "bundle"
        staging.mkdir()
        files = []
        total = 0
        for name in NAMES:
            source = assets / name
            if source.is_symlink() or not source.is_file():
                raise ValueError(f"asset is missing or not a regular file: {name}")
            size = source.stat().st_size
            if not 0 < size <= MAX_FILE:
                raise ValueError(f"asset size exceeds bounds: {name}")
            with source.open("rb") as stream:
                data = stream.read(MAX_FILE + 1)
            if len(data) != size:
                raise ValueError(f"asset changed while packaging: {name}")
            if name.endswith(".wasm") and not data.startswith(b"\0asm\x01\0\0\0"):
                raise ValueError("renderer binary is not a supported WASM module")
            total += size
            if total > MAX_TOTAL:
                raise ValueError("asset bundle exceeds total size bound")
            durable_file(staging / f"bmux-web-assets-{identity}-{name}", data)
            files.append({"name": name, "size": size, "sha256": hashlib.sha256(data).hexdigest()})
        manifest = {"schema": 1, "version": version, "renderer_sha256": identity,
                    "wasm_bindgen_version": bindgen, "files": files}
        encoded = (json.dumps(manifest, sort_keys=True, indent=2) + "\n").encode()
        if len(encoded) > 16 * 1024:
            raise ValueError("manifest exceeds size bound")
        durable_file(staging / f"bmux-web-assets-{identity}.json", encoded)
        sync_directory(staging)
        # Publish only the complete staged directory, on the same filesystem.
        staging.rename(output)
        sync_directory(output.parent)
    print(f"Release assets for {tag}: {output}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--assets", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    root = Path(__file__).resolve().parent.parent
    package(root, args.assets.resolve(), args.output.absolute())


if __name__ == "__main__":
    main()
