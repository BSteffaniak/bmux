#!/usr/bin/env python3
"""Build real API crates outside the repository's sibling-directory layout.

Run with `nix develop --command python3 scripts/check-relocated-schema-imports.py`.
Only temporary copies are edited; Cargo's configured toolchain/target directory
are preserved. The second build verifies provider BPDL edits invalidate bindings.
"""

import json
from pathlib import Path
import re
import shutil
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[1]
TARGETS = (
    "bmux_tab_bar_plugin_api",
    "bmux_recording_plugin_api",
    "bmux_decoration_plugin_api",
)


def main():
    metadata = json.loads(
        subprocess.check_output(
            ["cargo", "metadata", "--no-deps", "--format-version", "1"], cwd=ROOT
        )
    )
    packages = {
        Path(p["manifest_path"]).parent.resolve(): p for p in metadata["packages"]
    }
    selected = {}

    def visit(path):
        if path in selected:
            return
        package = packages[path]
        selected[path] = package
        for dependency in package["dependencies"]:
            if dependency.get("path"):
                visit(Path(dependency["path"]).resolve())

    for path, package in packages.items():
        if package["name"] in TARGETS:
            visit(path)

    with tempfile.TemporaryDirectory(prefix="bmux-relocated-schemas-") as directory:
        root = Path(directory)
        destinations = {
            path: root / f"isolated-{i}" / f"{package['name']}-{package['version']}"
            for i, (path, package) in enumerate(selected.items())
        }
        for path, destination in destinations.items():
            shutil.copytree(
                path, destination, ignore=shutil.ignore_patterns("target", ".git")
            )

        manifest = (ROOT / "Cargo.toml").read_text()
        members = ",\n".join(
            json.dumps(str(p.relative_to(root))) for p in destinations.values()
        )
        manifest = re.sub(
            r"(?ms)^members\s*=\s*\[.*?\]",
            f"members = [\n{members}\n]",
            manifest,
            count=1,
        )
        manifest = re.sub(r"(?ms)^default-members\s*=\s*\[.*?\]\s*", "", manifest)

        def relocate(match):
            original = (ROOT / match[1]).resolve()
            return f"path = {json.dumps(str(destinations.get(original, original)))}"

        manifest = re.sub(r'path\s*=\s*"([^"]+)"', relocate, manifest)
        (root / "Cargo.toml").write_text(manifest)
        shutil.copyfile(ROOT / "Cargo.lock", root / "Cargo.lock")
        # Prune the copied workspace lockfile once; subsequent builds are locked.
        subprocess.run(
            [
                "cargo",
                "generate-lockfile",
                "--offline",
                "--manifest-path",
                str(root / "Cargo.toml"),
            ],
            cwd=ROOT,
            check=True,
        )
        command = [
            "cargo",
            "check",
            "--offline",
            "--locked",
            "--manifest-path",
            str(root / "Cargo.toml"),
        ]
        for target in TARGETS:
            command += ["-p", target]
        # Keep Cargo config discovery anchored in the original workspace.
        subprocess.run(command, cwd=ROOT, check=True)

        provider = next(
            destinations[p]
            for p in selected
            if selected[p]["name"] == "bmux_workspaces_plugin_api"
        )
        schema = provider / "bpdl/workspaces-plugin.bpdl"
        original = schema.read_text()
        # Keep the provider valid but remove a type the tab-bar consumer uses.
        # Failure must come from re-expanding the consumer, not the provider.
        if "workspace-summary" not in original:
            raise RuntimeError("provider fixture no longer defines workspace-summary")
        schema.write_text(
            original.replace("workspace-summary", "relocated-workspace-summary")
        )
        result = subprocess.run(
            command, cwd=ROOT, capture_output=True, text=True, check=False
        )
        if (
            result.returncode == 0
            or "has no type `workspace-summary`" not in result.stderr
        ):
            raise RuntimeError(
                "provider schema edit did not invalidate consumer bindings\n"
                + result.stderr
            )
        schema.write_text(original)
        subprocess.run(command, cwd=ROOT, check=True)
        print("Relocated dependency imports and provider schema invalidation passed.")


if __name__ == "__main__":
    main()
