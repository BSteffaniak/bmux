#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

python3 - <<'PY'
import pathlib
import subprocess
import sys
import tomllib

# Include untracked manifests, but not ignored build artifacts or vendored trees.
paths = subprocess.check_output(
    ["git", "ls-files", "--cached", "--others", "--exclude-standard", "-z",
     "--", "Cargo.toml", "*Cargo.toml"]
).decode().split("\0")
manifests = {
    path: tomllib.loads(pathlib.Path(path).read_text())
    for path in sorted(set(paths)) if path and pathlib.Path(path).is_file()
}
workspace_dependencies = manifests["Cargo.toml"]["workspace"]["dependencies"]
local_packages = {
    manifest["package"]["name"]
    for manifest in manifests.values() if "package" in manifest
}
errors = []


def owning_dependencies(path):
    # Nested independent workspaces (the portable Mermaid guest) own their
    # inheritance tables; dependencies must never resolve against the host.
    for parent in pathlib.Path(path).parents:
        candidate = str(parent / "Cargo.toml")
        manifest = manifests.get(candidate)
        if manifest is not None and "workspace" in manifest:
            return manifest["workspace"].get("dependencies", {})
    return workspace_dependencies


def check(path, table, section=()):
    for key, value in table.items():
        current = section + (key,)
        if key in {"dependencies", "dev-dependencies", "build-dependencies"}:
            if current == ("workspace", "dependencies"):
                continue
            for name, dependency in value.items():
                spec = dependency if isinstance(dependency, dict) else {}
                location = f"{path} [{'.'.join(current)}] {name}"
                inherited = spec.get("workspace") is True
                if "path" in spec:
                    errors.append(f"{location}: define paths in workspace.dependencies only")
                if spec.get("package", name) in local_packages and not inherited:
                    errors.append(f"{location}: local packages must use workspace = true")
                if inherited and name not in owning_dependencies(path):
                    errors.append(f"{location}: missing workspace.dependencies definition")
        elif isinstance(value, dict):
            check(path, value, current)


for path, manifest in manifests.items():
    check(path, manifest)
if errors:
    print("\n".join(errors), file=sys.stderr)
    sys.exit(1)
print(f"Workspace dependency check passed ({len(manifests)} manifests)")
PY
