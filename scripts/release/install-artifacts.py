#!/usr/bin/env python3
"""Install one verified release inventory into an explicit directory (never PATH)."""
import argparse
import hashlib
import json
from pathlib import Path
import re
import shutil
import os
import tempfile
import uuid


def activate(source, prefix, version):
    """Publish a complete immutable version, then switch only the CLI symlink."""
    if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_.+-]*", version):
        raise ValueError("unsafe release version")
    prefix = prefix.resolve()
    versions = prefix / "lib/bmux/versions"
    versions.mkdir(parents=True, exist_ok=True)
    bin_dir = prefix / "bin"
    bin_dir.mkdir(parents=True, exist_ok=True)
    lock = versions / ".install-lock"
    lock.mkdir()  # Concurrent publishers fail explicitly; no silent lock stealing.
    try:
        destination = versions / version
        if destination.exists() or destination.is_symlink():
            raise ValueError("immutable release version already installed")
        with tempfile.TemporaryDirectory(prefix=".stage-", dir=versions) as staging:
            stage = Path(staging) / "payload"
            install(source, stage)
            if not (stage / "bmux").is_file():
                raise ValueError("activation requires a Unix BMUX artifact")
            for path in stage.iterdir():
                with path.open("rb") as file:
                    os.fsync(file.fileno())
            sync_directory(stage)
            os.rename(stage, destination)
            sync_directory(versions)
        link = bin_dir / f".bmux-{uuid.uuid4().hex}"
        try:
            link.symlink_to(destination / "bmux")
            os.replace(link, bin_dir / "bmux")
            sync_directory(bin_dir)
        finally:
            if link.is_symlink():
                link.unlink()
    finally:
        lock.rmdir()


def sync_directory(path):
    descriptor = os.open(path, os.O_RDONLY)
    try:
        os.fsync(descriptor)
    finally:
        os.close(descriptor)


def install(source, destination, binary_name=None):
    inventory = json.loads((source / "artifacts.json").read_text())
    if inventory.get("schema_version") != 1:
        raise ValueError("unsupported artifact inventory")
    names = set()
    files = []
    for item in inventory["artifacts"]:
        name = item["relative_path"]
        if not isinstance(name, str) or not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_.-]*", name):
            raise ValueError("unsafe artifact path")
        if name in names or name == "artifacts.json":
            raise ValueError("duplicate artifact path")
        names.add(name)
        path = source / name
        if path.is_symlink() or not path.is_file():
            raise ValueError(f"missing regular artifact: {path}")
        if path.stat().st_size != item["bytes"] or hashlib.sha256(path.read_bytes()).hexdigest() != item["sha256"]:
            raise ValueError(f"artifact identity mismatch: {path}")
        files.append((path, name))
    if not names.intersection({"bmux", "bmux.exe"}):
        raise ValueError("inventory contains no BMUX binary")
    if binary_name is not None:
        if not re.fullmatch(r"bmux-[A-Za-z0-9_.-]+", binary_name):
            raise ValueError("slot binary name must be bmux-<slot>")
        # Generic helpers have stable sibling names: two versions must not share
        # one physical directory. Explicit unique slot directory is mandatory.
        if destination.exists() and any(destination.iterdir()):
            raise ValueError("slot installation requires an empty dedicated directory")
    # Refuse existing artifact destinations before any copy; unrelated npm
    # package metadata may coexist, but no installed version is overwritten.
    for _, name in files:
        if binary_name and name in {"bmux", "bmux.exe"}:
            name = binary_name + (".exe" if name.endswith(".exe") else "")
        output = destination / name
        if output.exists() or output.is_symlink():
            raise ValueError(f"artifact destination already exists: {output}")
    for name in ["artifacts.json", "installation.json"]:
        if (destination / name).exists() or (destination / name).is_symlink():
            raise ValueError(f"inventory destination already exists: {destination / name}")
    destination.mkdir(parents=True, exist_ok=True)
    for path, name in files:
        if binary_name and name in {"bmux", "bmux.exe"}:
            name = binary_name + (".exe" if name.endswith(".exe") else "")
        output = destination / name
        if output.is_symlink():
            raise ValueError(f"refusing symlink destination: {output}")
        shutil.copyfile(path, output)
        output.chmod(0o755)
    # Preserve the original signed/checksummed names even for slot renaming;
    # --binary-name records the actual installed CLI name separately.
    shutil.copyfile(source / "artifacts.json", destination / "artifacts.json")
    if binary_name:
        (destination / "installation.json").write_text(json.dumps({"schema_version": 1,
            "binary_name": binary_name, "source_inventory": "artifacts.json"}, indent=2) + "\n")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("source", type=Path)
    parser.add_argument("destination", type=Path)
    parser.add_argument("--binary-name", help="Explicit slot CLI basename; requires empty destination")
    parser.add_argument("--activate-version", help="Publish immutable version and atomically activate destination/bin/bmux")
    args = parser.parse_args()
    if args.activate_version:
        if args.binary_name:
            parser.error("slot rename and global activation are mutually exclusive")
        activate(args.source, args.destination, args.activate_version)
    else:
        install(args.source, args.destination, args.binary_name)
