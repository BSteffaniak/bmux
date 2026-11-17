#!/usr/bin/env python3
"""Prepare real acceptance artifacts using the shared packager (integration only).

Without --build, only generate packaging sources under --output. Never creates a
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


def fixture_package(root: Path, output: Path, shared: dict) -> Path:
    """Package X2's exact entrypoint and sibling implementation, without copies of behavior."""
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
    cargo = f'''# Generated acceptance packaging; implementation remains X2-owned.
[package]
name = "bmux_acceptance_scoped_loader_native"
version = "0.0.0"
edition = "2024"
publish = false

[workspace]

[lib]
crate-type = ["cdylib"]
path = "src/scoped_loader_native.rs"

[dependencies]
bmux_plugin_sdk = {{ path = {shared['quote'](str(sdk))}, default-features = false }}
bmux_plugin = {{ path = {shared['quote'](str(host))}, default-features = false }}
tokio = {shared['toml_value'](tokio)}
serde_json = {shared['toml_value'](serde)}
'''
    implementation = output / "implementation"
    implementation.mkdir(parents=True, exist_ok=True)
    (implementation / "lib.rs").write_bytes((source / "scoped_loader_definition.rs").read_bytes())
    implementation_cargo = cargo.replace('name = "bmux_acceptance_scoped_loader_native"',
                                         'name = "bmux_acceptance_scoped_loader_definition"')
    implementation_cargo = implementation_cargo.replace('crate-type = ["cdylib"]', 'crate-type = ["rlib"]')
    implementation_cargo = implementation_cargo.replace('path = "src/scoped_loader_native.rs"', 'path = "lib.rs"')
    implementation_cargo = implementation_cargo.replace('[workspace]\n', '')
    # The implementation is a member of the wrapper workspace, not another
    # workspace root. Root-only patches are added to the wrapper below.
    (implementation / "Cargo.toml").write_text(implementation_cargo)
    cargo = cargo.replace('[workspace]\n', '[workspace]\nmembers = ["implementation"]\n')
    cargo = cargo.replace(f'serde_json = {shared["toml_value"](serde)}',
                          'scoped_loader_definition = { package = "bmux_acceptance_scoped_loader_definition", path = "implementation" }')
    (output / "src").mkdir(parents=True, exist_ok=True)
    (output / "src/scoped_loader_native.rs").write_text(
        "// Generated thin wrapper; implementation owns all behavior.\n"
        "bmux_plugin_sdk::export_plugin_entrypoint!(scoped_loader_definition::plugin_definition);\n")
    cargo += shared["workspace_patches"](sdk / "Cargo.toml")
    (output / "Cargo.toml").write_text(cargo)
    (output / "packaging.json").write_text(json.dumps({
        "schema_version": 1, "expected_id": "acceptance.scoped-loader",
        "source": str(source), "roles": ["backend", "frontend"],
        "abi_version": shared["ABI_VERSION"],
        "required_capabilities": shared["REQUIRED_CAPABILITIES"],
    }, indent=2) + "\n")
    return output / "Cargo.toml"


def main() -> None:
    root = Path(__file__).resolve().parents[1]
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bcode-root", type=Path, required=True,
                        help="approved Bcode worktree containing the real file-references implementation")
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
    bcode = args.bcode_root.resolve()
    artifacts = []
    fixture = output / "scoped-loader"
    references = output / "file-references"
    specifications = [
        ("acceptance.scoped-loader", fixture, lambda: fixture_package(root, fixture, shared)),
        ("example.hello", output / "hello", lambda: shared["generate"](
            root / "examples/hello-plugin/Cargo.toml",
            root / "packages/plugin-sdk/Cargo.toml", output / "hello", "backend", [], str(uuid.uuid4()))),
        ("bcode.file-references", references, lambda: shared["generate"](
            bcode / "plugins/file-references-plugin/Cargo.toml",
            root / "packages/plugin-sdk/Cargo.toml", references, "backend", [], str(uuid.uuid4()))),
    ]
    for domain in ("filesystem", "shell"):
        directory = output / domain
        specifications.append((f"bcode.{domain}", directory,
            lambda domain=domain, directory=directory: shared["generate"](
                bcode / f"plugins/{domain}-plugin/Cargo.toml",
                root / "packages/plugin-sdk/Cargo.toml", directory, "backend", [], str(uuid.uuid4()))))
    for expected, directory, generate in specifications:
        with shared["packaging_owner"](directory):
            manifest = generate()
            print(manifest)
            if args.build:
                library = shared["build"](manifest, args.release, args.target)
                artifacts.append({"expected_id": expected, "path": str(library.resolve())})
    if not args.build:
        return
    # Record exact Cargo output, never derive a platform-specific library filename.
    environment = {"BMUX_ACCEPTANCE_NATIVE_ARTIFACT": artifacts[0]["path"]}
    for artifact in artifacts:
        if artifact["expected_id"] in {"bcode.filesystem", "bcode.shell"}:
            domain = artifact["expected_id"].split(".")[1].upper()
            environment[f"BCODE_{domain}_WRAPPER_ARTIFACT"] = artifact["path"]
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
