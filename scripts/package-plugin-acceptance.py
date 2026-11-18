#!/usr/bin/env python3
"""Prepare real acceptance artifacts using the shared packager (integration only).

Every artifact is a crate that exports its own entry (`plugin!` + role
features), built directly; a crate without the role feature fails to build.
Without --build, only the fixture package is generated under --output. Never creates a
workspace member, installs an artifact, or modifies test fixtures. --build is an
explicit later-phase action; no test or native artifact is executed by default.
"""
import argparse
import json
from pathlib import Path
import runpy
import subprocess
import sys
import tomllib
import uuid


def quote(value: str) -> str:
    """JSON strings are also valid TOML basic strings for these path/name inputs."""
    return json.dumps(value, ensure_ascii=False)


def toml_value(value: object) -> str:
    if isinstance(value, str):
        return quote(value)
    if isinstance(value, bool):
        return "true" if value else "false"
    if isinstance(value, list):
        return "[" + ", ".join(toml_value(item) for item in value) + "]"
    if isinstance(value, dict):
        return "{ " + ", ".join(f"{quote(key)} = {toml_value(item)}" for key, item in value.items()) + " }"
    if isinstance(value, int):
        return str(value)
    raise ValueError(f"unsupported Cargo patch value: {type(value).__name__}")


def workspace_patches(implementation: Path) -> str:
    """Carry workspace source selection into the standalone fixture workspace."""
    for parent in implementation.parents:
        candidate = parent / "Cargo.toml"
        if candidate.exists():
            workspace = tomllib.loads(candidate.read_text())
            if "workspace" in workspace:
                patches = workspace.get("patch", {})
                break
    else:
        patches = {}
    lines = []
    for registry, dependencies in patches.items():
        lines.append(f"\n[patch.{quote(registry)}]")
        for name, dependency in dependencies.items():
            dependency = dict(dependency)
            if "path" in dependency:
                dependency["path"] = str((parent / dependency["path"]).resolve())
            lines.append(f"{quote(name)} = {toml_value(dependency)}")
    return "\n".join(lines) + "\n"


def fixture_package(root: Path, output: Path, shared: dict) -> Path:
    """Package X2's exact fixture sources as one crate that exports its own entry.

    The fixture is test input, not a workspace member, so it gets a generated
    package; but there is no wrapper: the crate holds the fixture's entry and
    implementation files and is built directly with ``cargo rustc``.
    """
    if output.exists() and any(output.iterdir()) and not (output / "packaging.json").is_file():
        raise ValueError("refusing to overwrite a non-generated fixture directory")
    source = root / "packages/plugin/tests/fixtures"
    workspace = tomllib.loads((root / "Cargo.toml").read_text())
    serde = workspace["workspace"]["dependencies"]["serde_json"]
    if isinstance(serde, dict) and "path" in serde:
        serde = {**serde, "path": str((root / serde["path"]).resolve())}
    sdk = root / "packages/plugin-sdk"
    # The fixture exercises the activation event bus through the host crate's
    # `EventBus`, whose subscriptions are tokio broadcast receivers.
    host = root / "packages/plugin"
    tokio = {**workspace["workspace"]["dependencies"]["tokio"], "features": ["sync"]}
    cargo = f'''# Generated acceptance package; the fixture sources are X2-owned.
[package]
name = "bmux_acceptance_scoped_loader_native"
version = "0.0.0"
edition = "2024"
publish = false

[workspace]

[lib]
crate-type = ["rlib"]
path = "src/scoped_loader_native.rs"

[dependencies]
bmux_plugin_sdk = {{ path = {quote(str(sdk))}, default-features = false }}
bmux_plugin = {{ path = {quote(str(host))}, default-features = false }}
tokio = {toml_value(tokio)}
serde_json = {toml_value(serde)}

[features]
# The fixture exports its two-role definition unconditionally; the packager
# selects the backend build.
dynamic-backend = []
dynamic-frontend = []
'''
    cargo += workspace_patches(sdk / "Cargo.toml")
    (output / "src").mkdir(parents=True, exist_ok=True)
    for name in ("scoped_loader_native.rs", "scoped_loader_definition.rs"):
        (output / "src" / name).write_bytes((source / name).read_bytes())
    (output / "Cargo.toml").write_text(cargo)
    # Same registry versions as the host under test.
    (output / "Cargo.lock").write_bytes((root / "Cargo.lock").read_bytes())
    (output / "packaging.json").write_text(json.dumps({
        "schema_version": 2, "expected_id": "acceptance.scoped-loader",
        "source": str(source), "roles": ["backend", "frontend"],
        "abi_version": shared["ABI_VERSION"],
        "required_capabilities": shared["REQUIRED_CAPABILITIES"],
    }, indent=2) + "\n")
    return output / "Cargo.toml"


def main() -> None:
    root = Path(__file__).resolve().parents[1]
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bcode-root", type=Path,
                        help="approved Bcode worktree whose plugin crates package themselves; "
                             "without it only the BMUX artifacts are built")
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--build", action="store_true")
    parser.add_argument("--release", action="store_true")
    parser.add_argument("--target")
    parser.add_argument("--run-acceptance", action="store_true",
                        help="after source convergence: inspect trusted artifacts and run X2's identity harness")
    args = parser.parse_args()
    if args.run_acceptance and not args.build:
        parser.error("--run-acceptance requires --build")
    shared = runpy.run_path(str(root / "scripts/package-plugin.py"))
    output = args.output.resolve()
    artifacts = []
    fixture = output / "scoped-loader"

    def direct(manifest: Path):
        """Crates that export their own entry: nothing to generate, build directly."""
        def build() -> Path:
            library, _ = shared["build_direct"](manifest, "backend", [], args.release, args.target,
                                                output / "dylib-target", str(uuid.uuid4()))
            return library
        return build

    specifications = [
        ("acceptance.scoped-loader", fixture,
         lambda: direct(fixture_package(root, fixture, shared))),
        ("example.hello", output / "hello",
         lambda: direct(root / "examples/hello-plugin/Cargo.toml")),
    ]
    if args.bcode_root:
        bcode = args.bcode_root.resolve()
        for domain in ("file-references", "filesystem", "shell"):
            specifications.append((f"bcode.{domain}", output / domain,
                lambda domain=domain: direct(bcode / f"plugins/{domain}-plugin/Cargo.toml")))
    for expected, directory, prepare in specifications:
        with shared["packaging_owner"](directory):
            build = prepare()
            if args.build:
                library = build()
                artifacts.append({"expected_id": expected, "path": str(library.resolve())})
    if not args.build:
        return
    # Record exact Cargo output, never derive a platform-specific library filename.
    environment = {"BMUX_ACCEPTANCE_NATIVE_ARTIFACT": artifacts[0]["path"]}
    for artifact in artifacts:
        if artifact["expected_id"] in {"bcode.filesystem", "bcode.shell"}:
            domain = artifact["expected_id"].split(".")[1].upper()
            environment[f"BCODE_{domain}_BACKEND_ARTIFACT"] = artifact["path"]
    report = {"schema_version": 1, "artifacts": artifacts,
              "abi_version": shared["ABI_VERSION"],
              "required_capabilities": shared["REQUIRED_CAPABILITIES"],
              "environment": environment}
    output.mkdir(parents=True, exist_ok=True)
    (output / "acceptance-artifacts.json").write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report, indent=2))
    if args.run_acceptance:
        for artifact in artifacts:
            observed = shared["artifact_metadata"](Path(artifact["path"]))
            if observed["metadata"].get("plugin_id") != artifact["expected_id"]:
                raise RuntimeError(f"independent embedded identity mismatch: {artifact}")
        command = [sys.executable, str(root / "scripts/shared-plugin-artifact-acceptance.py")]
        for artifact in artifacts:
            command.extend(["--artifact", f'{artifact["expected_id"]}={artifact["path"]}'])
        command.extend(["--report", str(output / "identity-acceptance.json")])
        subprocess.run(command, check=True, cwd=root)


if __name__ == "__main__":
    main()
