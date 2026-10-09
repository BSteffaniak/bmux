#!/usr/bin/env python3
"""Build and stage declared plugin executables; never discover executables on PATH."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tomllib

NAME = re.compile(r"[A-Za-z0-9][A-Za-z0-9_.-]*\Z")
FIELDS = {"id", "cargo_package", "cargo_binary", "relative_path", "protocol_version"}


def declarations(workspace):
    result = []
    destinations = set()
    for manifest in sorted((workspace / "plugins").glob("*/plugin.toml")):
        data = tomllib.loads(manifest.read_text())
        for item in data.get("auxiliary_executables", []):
            if set(item) != FIELDS:
                raise ValueError(f"{manifest}: unsupported auxiliary executable fields")
            if any(not isinstance(item[key], str) or not NAME.fullmatch(item[key])
                   for key in FIELDS - {"protocol_version"}):
                raise ValueError(f"{manifest}: unsafe executable name")
            version = item["protocol_version"]
            if type(version) is not int or version <= 0:
                raise ValueError(f"{manifest}: invalid protocol version")
            destination = item["relative_path"]
            if destination in destinations or destination in {"bmux", "bmux.exe", "artifacts.json", "install-artifacts.py", "installation.json"}:
                raise ValueError(f"{manifest}: colliding destination {destination}")
            destinations.add(destination)
            result.append({"owner": data["id"], **item})
    return result


def supported(target):
    # This release contract supports Unix auxiliary executables only. Never ship
    # an unrelated/stub Windows artifact while claiming supervisor availability.
    return "-apple-darwin" in target or "-unknown-linux-" in target


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("operation", choices=["build", "stage", "nfpm"])
    parser.add_argument("--workspace", type=Path, default=Path(__file__).resolve().parents[2])
    parser.add_argument("--target", required=True)
    parser.add_argument("--artifact-dir", type=Path)
    parser.add_argument("--destination", type=Path)
    parser.add_argument("--config", type=Path)
    args = parser.parse_args()
    items = declarations(args.workspace)
    unix = supported(args.target)
    if not unix and args.target != "x86_64-pc-windows-msvc":
        raise ValueError(f"unsupported release target: {args.target}")
    if args.operation == "build":
        for item in items if unix else []:
            subprocess.run(["cargo", "build", "--locked", "--release", "--target", args.target,
                            "-p", item["cargo_package"], "--bin", item["cargo_binary"]],
                           cwd=args.workspace, check=True)
        return
    if args.operation == "nfpm":
        if not unix or args.config is None or args.destination is None:
            parser.error("nfpm requires a Unix target, --config and --destination")
        text = args.config.read_text()
        for item in items:
            name = item["relative_path"]
            text += f"  - src: ./dist/${{TARGET}}/{name}\n    dst: /usr/bin/{name}\n    file_info:\n      mode: 0755\n"
        text += "  - src: ./dist/${TARGET}/install-artifacts.py\n    dst: /usr/share/bmux/install-artifacts.py\n    file_info:\n      mode: 0644\n"
        text += "  - src: ./dist/${TARGET}/artifacts.json\n    dst: /usr/share/bmux/artifacts.json\n    file_info:\n      mode: 0644\n"
        args.destination.write_text(text)
        return
    if args.artifact_dir is None or args.destination is None:
        parser.error("stage requires --artifact-dir and --destination")
    if args.destination.exists() and any(args.destination.iterdir()):
        raise ValueError("staging destination must be empty")
    binary = "bmux" if unix else "bmux.exe"
    selected = [(binary, binary, None)]
    # Package the installer as an authenticated inventory member, not remote code
    # fetched separately during installation.
    installer = Path(__file__).with_name("install-artifacts.py")
    selected += [(installer, "install-artifacts.py", None)]
    selected += [(item["cargo_binary"], item["relative_path"], item) for item in items if unix]
    sources = []
    for source, destination, item in selected:
        path = args.artifact_dir / source
        if not path.is_file() or path.is_symlink():
            raise ValueError(f"missing regular build artifact: {path}")
        if unix and destination != "install-artifacts.py" and not os.access(path, os.X_OK):
            raise ValueError(f"build artifact is not executable: {path}")
        sources.append((path, destination, item))
    args.destination.mkdir(parents=True, exist_ok=True)
    inventory = {"schema_version": 1, "target": args.target, "artifacts": [],
                 "unavailable_auxiliary_executables": [] if unix else items}
    for source, destination, item in sources:
        path = args.destination / destination
        shutil.copyfile(source, path)
        path.chmod(0o755)
        inventory["artifacts"].append({"relative_path": destination,
            "sha256": hashlib.sha256(path.read_bytes()).hexdigest(),
            "bytes": path.stat().st_size, **({"declaration": item} if item else {})})
    (args.destination / "artifacts.json").write_text(json.dumps(inventory, indent=2) + "\n")


if __name__ == "__main__":
    main()
