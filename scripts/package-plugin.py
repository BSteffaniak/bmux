#!/usr/bin/env python3
"""Package BMUX plugins as dynamic libraries; inspect and install artifacts.

A plugin crate that declares the role features ``dynamic-backend`` /
``dynamic-frontend`` packages itself: ``plugin!`` exports the entry symbol only
under ``--cfg bmux_plugin_dylib`` plus the selected role feature, and this
script builds the crate directly with ``cargo rustc --crate-type cdylib`` in its
own workspace (its own Cargo.lock and dependency graph, so a git-pinned SDK is
used exactly as resolved). No wrapper crate is generated; a crate without the
role feature for the requested component cannot be packaged.

Python 3.11+. Build output is obtained from Cargo JSON, never guessed from
target/debug or host operating system suffixes.
"""
from __future__ import annotations

import argparse
import ctypes
from contextlib import contextmanager
import hashlib
import json
import os
from pathlib import Path
import sys
import subprocess
import tempfile
import tomllib
import uuid

ABI_VERSION = 3
CAPABILITIES_BY_ABI = {2: 15, 3: 31}
# Explicit supported packaging profiles, not the SDK's broader subset validator.
# 2047 adds callback authority, which the SDK descriptor advertises and the
# host requires for selected lifecycle activation. 4095 adds host wake v1
# (bit 2048), which requires the size-checked `read_wake` descriptor tail.
SUPPORTED_CAPABILITIES_BY_ABI = {2: frozenset({15}), 3: frozenset({31, 127, 255, 511, 1023, 2047, 4095})}
CAP_HOST_WAKE_V1 = 2048
REQUIRED_CAPABILITIES = CAPABILITIES_BY_ABI[ABI_VERSION]


def component_features(manifest: dict, role: str, extra: list[str]) -> list[str]:
    """Features that make up one component, declared by the implementation itself.

    `[package.metadata.bmux-plugin.components.<role>] features = [...]` is the
    single source of truth, so every packager caller builds the same component;
    explicit --feature values extend it. Packaged builds disable default
    features, so an implementation that gates its plugin definition behind a
    feature must declare it here.
    """
    declared = (manifest.get("package", {}).get("metadata", {}).get("bmux-plugin", {})
                .get("components", {}).get(role, {}).get("features", []))
    if not isinstance(declared, list) or not all(isinstance(item, str) for item in declared):
        raise ValueError(f"package.metadata.bmux-plugin.components.{role}.features must be a list of strings")
    return list(dict.fromkeys([*declared, *extra]))


DYNAMIC_MODE_CFG = "bmux_plugin_dylib"


def dynamic_mode_environment(base: dict[str, str] | None = None) -> dict[str, str]:
    """Select plug-rt dynamic mode (``--cfg bmux_plugin_dylib``) for a dylib build.

    Dynamic mode routes the plugin's spawns and timers through the host's
    runtime table instead of Tokio. It is a compiler cfg, never a Cargo
    feature, and is applied only to dylib builds, which own their target
    directory (see ``direct_target_dir``), so a workspace build can never
    unify it into the host. Existing flags are preserved; Cargo prefers
    ``CARGO_ENCODED_RUSTFLAGS`` over ``RUSTFLAGS`` when both are set.
    """
    environment = dict(os.environ if base is None else base)
    encoded = environment.get("CARGO_ENCODED_RUSTFLAGS")
    if encoded is not None:
        flags = [flag for flag in encoded.split("\x1f") if flag]
        environment["CARGO_ENCODED_RUSTFLAGS"] = "\x1f".join(flags + ["--cfg", DYNAMIC_MODE_CFG])
    else:
        flags = environment.get("RUSTFLAGS", "").strip()
        environment["RUSTFLAGS"] = f"{flags} --cfg {DYNAMIC_MODE_CFG}".strip()
    return environment


ROLE_FEATURE = "dynamic-{}"
SDK_PACKAGE = "bmux_plugin_sdk"


def packages_itself(manifest: dict, role: str) -> bool:
    """Whether the crate exports its own entry for ``role`` (``plugin!`` + role feature)."""
    return ROLE_FEATURE.format(role) in manifest.get("features", {})


def cargo_metadata(manifest: Path) -> dict:
    """Resolve the crate's own workspace: its Cargo.lock and dependency graph."""
    result = subprocess.run(
        ["cargo", "metadata", "--format-version", "1", "--manifest-path", str(manifest)],
        check=True, stdout=subprocess.PIPE, text=True)
    return json.loads(result.stdout)


def resolved_package(metadata: dict, manifest: Path) -> dict:
    for package in metadata["packages"]:
        if Path(package["manifest_path"]).resolve() == manifest.resolve():
            return package
    raise RuntimeError(f"cargo metadata does not list {manifest}")


def sdk_source(metadata: dict, package: dict) -> str:
    """The SDK the crate actually links, as its own lockfile resolved it.

    Path dependencies report ``path+file://...``; git dependencies report the
    exact ``git+...#<commit>``; registry dependencies their registry. Nothing
    is substituted: the artifact speaks the ABI of this SDK.
    """
    nodes = {node["id"]: node for node in metadata.get("resolve", {}).get("nodes", [])}
    packages = {candidate["id"]: candidate for candidate in metadata["packages"]}
    node = nodes.get(package["id"])
    if node is None:
        raise RuntimeError(f"no resolved dependency graph for {package['name']}")
    for dependency in node["deps"]:
        candidate = packages.get(dependency["pkg"])
        if candidate and candidate["name"] == SDK_PACKAGE:
            return candidate.get("source") or f"path+file://{Path(candidate['manifest_path']).parent}"
    raise RuntimeError(f"{package['name']} does not depend on {SDK_PACKAGE}; it cannot export a plugin entry")


def direct_target_dir(metadata: dict, cargo_target_dir: Path | None) -> Path:
    """A target directory for dylib builds only.

    The cfg changes every crate's compiler flags, so sharing the workspace's
    own target directory would rebuild everything back and forth.
    """
    return (cargo_target_dir or Path(metadata["target_directory"]) / "plugin-dylib").resolve()


def build_direct(implementation: Path, role: str, features: list[str], release: bool,
                 target: str | None, cargo_target_dir: Path | None, artifact_id: str,
                 locked: bool = False) -> tuple[Path, dict]:
    """Build the crate itself as a cdylib for one role; return the library and provenance."""
    manifest = tomllib.loads(implementation.read_text())
    if not packages_itself(manifest, role):
        raise ValueError(f"{implementation} does not declare the {ROLE_FEATURE.format(role)!r} feature")
    metadata = cargo_metadata(implementation)
    package = resolved_package(metadata, implementation)
    sdk = sdk_source(metadata, package)
    selected = component_features(manifest, role, features)
    selected = list(dict.fromkeys([*selected, ROLE_FEATURE.format(role)]))
    target_dir = direct_target_dir(metadata, cargo_target_dir)
    command = ["cargo", "rustc", "--manifest-path", str(implementation),
               "--package", package["name"], "--lib", "--crate-type", "cdylib",
               "--no-default-features", "--features", ",".join(selected),
               "--target-dir", str(target_dir), "--message-format=json-render-diagnostics"]
    if release:
        command.append("--release")
    if target:
        command.extend(["--target", target])
    if locked:
        command.append("--locked")
    environment = dynamic_mode_environment()
    environment["BMUX_PLUGIN_ARTIFACT_ID"] = artifact_id
    artifacts = set()
    with subprocess.Popen(command, stdout=subprocess.PIPE, text=True, env=environment) as process:
        assert process.stdout is not None
        for line in process.stdout:
            message = json.loads(line)
            if message.get("reason") == "compiler-message":
                rendered = message.get("message", {}).get("rendered")
                if rendered:
                    print(rendered, end="", file=sys.stderr)
            if (message.get("reason") == "compiler-artifact"
                    and message.get("package_id") == package["id"]
                    and "cdylib" in message.get("target", {}).get("crate_types", [])):
                artifacts.update(Path(name) for name in message["filenames"]
                                 if Path(name).suffix in {".so", ".dylib", ".dll"})
        if process.wait() != 0:
            raise RuntimeError("plugin build failed; nothing installed")
    if len(artifacts) != 1:
        raise RuntimeError(f"expected one plugin library, received {len(artifacts)}")
    provenance = {"package": package["name"], "version": package["version"],
                  "component": role, "features": selected, "sdk_source": sdk,
                  "artifact_id": artifact_id}
    return artifacts.pop(), provenance


def sync_directory(path: Path) -> None:
    """Persist publication on platforms supporting directory fsync."""
    if os.name == "nt":
        raise RuntimeError("durable directory publication is not implemented on Windows; build artifacts without --install-root")
    descriptor = os.open(path, os.O_RDONLY)
    try:
        os.fsync(descriptor)
    finally:
        os.close(descriptor)


def artifact_metadata(library: Path) -> dict:
    """Inspect the selected trusted native artifact without creating an instance.

    Uses the SDK's frozen v2 descriptor prefix and bounded metadata operation.
    Loading trusted native code can run library initializers; this is not a
    sandbox and must never be used on untrusted discovery candidates.
    """
    class Header(ctypes.Structure):
        _fields_ = [("size", ctypes.c_uint32), ("version", ctypes.c_uint32)]

    class Descriptor(ctypes.Structure):
        _fields_ = Header._fields_ + [("capabilities", ctypes.c_uint64)] + [
            (name, ctypes.c_void_p) for name in
            ("start_scoped", "metadata", "create", "destroy", "start", "read", "cancel", "ack")]

    class DescriptorV3(ctypes.Structure):
        _fields_ = [("base", Descriptor), ("declarations", ctypes.c_void_p)]

    class DescriptorV3HostWake(ctypes.Structure):
        _fields_ = [("base", DescriptorV3), ("read_wake", ctypes.c_void_p)]

    native = ctypes.CDLL(str(library.resolve(strict=True)))
    entry = native.bmux_plugin_entry_v2
    entry.argtypes = []
    entry.restype = ctypes.c_void_p
    pointer = entry()
    if not pointer:
        raise RuntimeError("null shared ABI descriptor")
    header = ctypes.cast(pointer, ctypes.POINTER(Header)).contents
    expected_capabilities = CAPABILITIES_BY_ABI.get(header.version)
    minimum_size = ctypes.sizeof(DescriptorV3 if header.version == 3 else Descriptor)
    if expected_capabilities is None or header.size < minimum_size:
        raise RuntimeError("unsupported shared ABI descriptor version/size")
    descriptor = ctypes.cast(pointer, ctypes.POINTER(Descriptor)).contents
    if descriptor.capabilities not in SUPPORTED_CAPABILITIES_BY_ABI[header.version]:
        raise RuntimeError("shared descriptor capabilities do not match the negotiated ABI")
    if descriptor.capabilities & CAP_HOST_WAKE_V1 and (
            header.size < ctypes.sizeof(DescriptorV3HostWake)
            or not ctypes.cast(pointer, ctypes.POINTER(DescriptorV3HostWake)).contents.read_wake):
        raise RuntimeError("host wake capability requires the complete read_wake descriptor tail")
    if header.version == 3 and not ctypes.cast(pointer, ctypes.POINTER(DescriptorV3)).contents.declarations:
        raise RuntimeError("incomplete shared ABI declarations operation")
    if not all(getattr(descriptor, field) for field in
               ("start_scoped", "metadata", "create", "destroy", "start", "read", "cancel", "ack")):
        raise RuntimeError("incomplete shared ABI operation table")
    callback = ctypes.CFUNCTYPE(ctypes.c_int32, ctypes.c_void_p, ctypes.c_size_t,
                                ctypes.POINTER(ctypes.c_size_t))(descriptor.metadata)
    required = ctypes.c_size_t()
    status = callback(None, 0, ctypes.byref(required))
    if status != 3 or not 0 < required.value <= 1024 * 1024:
        raise RuntimeError("invalid or oversized shared metadata")
    capacity = required.value
    buffer = ctypes.create_string_buffer(capacity)
    if callback(buffer, capacity, ctypes.byref(required)) != 0 or not 0 < required.value <= capacity:
        raise RuntimeError("shared metadata retrieval failed")
    metadata = json.loads(buffer.raw[:required.value])
    if not isinstance(metadata, dict):
        raise RuntimeError("shared metadata must be an object")
    return {"version": header.version, "capabilities": descriptor.capabilities,
            "metadata": metadata}


def discover(root: Path) -> list[dict]:
    """Read/hash immutable receipts without loading executable code or selecting it."""
    found = []
    for receipt_path in sorted(root.glob("*/*/*/artifact.json")):
        if len(found) >= 4096:
            raise RuntimeError("artifact inventory limit exceeded")
        if receipt_path.stat().st_size > 2 * 1024 * 1024:
            raise RuntimeError(f"oversized artifact receipt: {receipt_path}")
        receipt = json.loads(receipt_path.read_text())
        if receipt.get("schema_version") != 1:
            raise RuntimeError(f"unsupported artifact receipt: {receipt_path}")
        for key, digest_key in (("library", "sha256"), ("declaration", "declaration_sha256")):
            name = receipt[key]
            if not name or Path(name).name != name or name in {".", ".."}:
                raise RuntimeError(f"invalid artifact path: {receipt_path}")
            path = receipt_path.parent / name
            if path.is_symlink() or hashlib.sha256(path.read_bytes()).hexdigest() != receipt[digest_key]:
                raise RuntimeError(f"artifact integrity mismatch: {path}")
        if (receipt["component"] != receipt_path.parents[1].name
                or receipt["plugin_id"] != receipt_path.parents[2].name
                or receipt_path.parent.name != f'{receipt["sha256"]}-{receipt["declaration_sha256"]}'):
            raise RuntimeError(f"artifact identity/layout mismatch: {receipt_path}")
        embedded = receipt["shared_descriptor"]["metadata"]
        declaration = tomllib.loads((receipt_path.parent / receipt["declaration"]).read_text())
        descriptor_version = receipt["shared_descriptor"]["version"]
        if (descriptor_version not in CAPABILITIES_BY_ABI
                or receipt["shared_descriptor"]["capabilities"] not in SUPPORTED_CAPABILITIES_BY_ABI[descriptor_version]
                or embedded.get("plugin_id") != receipt["plugin_id"]
                or embedded.get("roles") != [receipt["component"]]
                or embedded.get("version") != declaration["version"]
                or embedded.get("manifest_toml") != (receipt_path.parent / receipt["declaration"]).read_text()
                or sorted(embedded.get("required_capabilities", [])) != sorted(declaration.get("required_capabilities", []))):
            raise RuntimeError(f"artifact descriptor/declaration mismatch: {receipt_path}")
        found.append({**receipt, "receipt_path": str(receipt_path.resolve())})
    return found


DECLARATION_NAME = "plugin.toml"


def declaration_of(library: Path) -> str:
    """The sidecar declaration published for a built artifact: its embedded manifest."""
    manifest_toml = artifact_metadata(library)["metadata"].get("manifest_toml")
    if not isinstance(manifest_toml, str) or not manifest_toml:
        raise RuntimeError("artifact embeds no generated manifest")
    return manifest_toml


def install(library: Path, root: Path, component: str, artifact_id: str) -> Path:
    """Publish one immutable artifact directory, leaving selection to the host.

    The sidecar declaration is the manifest the plugin generated from its BPDL
    world and embedded in the artifact; the host reads it without running
    plugin code and later verifies it against the embedded copy. The receipt
    records observed bytes, not proof of ABI compatibility. No active plugin,
    selection file, daemon or prior artifact is replaced or removed.
    """
    if os.name == "nt":
        raise RuntimeError("durable installation is not implemented on Windows")
    library_bytes = library.read_bytes()
    observed = artifact_metadata(library)
    embedded = observed["metadata"]
    if library.read_bytes() != library_bytes:
        raise RuntimeError("artifact changed during descriptor discovery")
    manifest_toml = embedded.get("manifest_toml")
    if not isinstance(manifest_toml, str) or not manifest_toml:
        raise RuntimeError("artifact embeds no generated manifest")
    declaration_bytes = manifest_toml.encode()
    try:
        metadata = tomllib.loads(manifest_toml)
    except tomllib.TOMLDecodeError as error:
        raise RuntimeError("artifact embeds an invalid manifest") from error
    plugin_id = metadata["id"]
    if not plugin_id or any(c not in "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789._-" for c in plugin_id):
        raise ValueError("plugin id must be a safe single path component")
    if plugin_id in {".", ".."}:
        raise ValueError("invalid plugin id")
    if embedded.get("plugin_id") != plugin_id or embedded.get("version") != metadata["version"]:
        raise RuntimeError("embedded plugin identity/version differs from declaration")
    if embedded.get("roles") != [component]:
        raise RuntimeError("embedded component role differs from requested package")
    if sorted(embedded.get("required_capabilities", [])) != sorted(metadata.get("required_capabilities", [])):
        raise RuntimeError("embedded required capabilities differ from declaration")
    digest = hashlib.sha256(library_bytes).hexdigest()
    manifest_digest = hashlib.sha256(declaration_bytes).hexdigest()
    parent = root / plugin_id / component
    missing_parents = []
    ancestor = parent
    while not ancestor.exists():
        missing_parents.append(ancestor)
        ancestor = ancestor.parent
    parent.mkdir(parents=True, exist_ok=True)
    for created in reversed(missing_parents):
        sync_directory(created.parent)
    destination = parent / f"{digest}-{manifest_digest}"
    receipt = {"schema_version": 1, "plugin_id": plugin_id, "component": component,
               "artifact_id": artifact_id, "sha256": digest,
               "declaration_sha256": manifest_digest, "library": library.name,
               "declaration": DECLARATION_NAME, "shared_descriptor": observed}
    if destination.exists():
        if (json.loads((destination / "artifact.json").read_text()) != receipt
                or hashlib.sha256((destination / library.name).read_bytes()).hexdigest() != digest
                or (destination / DECLARATION_NAME).read_bytes() != declaration_bytes):
            raise RuntimeError(f"existing immutable artifact differs: {destination}")
        sync_directory(destination)
        sync_directory(parent)
        return destination
    with tempfile.TemporaryDirectory(prefix=".pending-", dir=parent) as temporary:
        stage = Path(temporary) / "artifact"
        stage.mkdir()
        (stage / library.name).write_bytes(library_bytes)
        # The embedded generated manifest is written verbatim. artifact.json resolves
        # the platform-specific library; old loaders must not guess this layout.
        (stage / DECLARATION_NAME).write_bytes(declaration_bytes)
        (stage / "artifact.json").write_text(json.dumps(receipt, indent=2) + "\n")
        for file in stage.iterdir():
            with file.open("rb") as stream:
                os.fsync(stream.fileno())
        sync_directory(stage)
        try:
            stage.rename(destination)
        except OSError:
            if not destination.exists():
                raise
            # A concurrent publisher must agree exactly, not merely share a name.
            return install(library, root, component, artifact_id)
        sync_directory(parent)
    return destination


@contextmanager
def packaging_owner(output: Path):
    """One writer/build per generated target; never steal uncertain ownership."""
    output.parent.mkdir(parents=True, exist_ok=True)
    lock = output.parent / f".{output.name}.packaging-owner"
    try:
        descriptor = os.open(lock, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    except FileExistsError as error:
        raise RuntimeError(f"packaging target already owned: {lock}; investigate the owner before removing stale evidence") from error
    try:
        with os.fdopen(descriptor, "w") as stream:
            stream.write(json.dumps({"pid": os.getpid(), "output": str(output)}))
            stream.flush()
            os.fsync(stream.fileno())
        yield
    finally:
        lock.unlink()


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--implementation-manifest", type=Path)
    parser.add_argument("--output", type=Path,
                        help="provenance receipt directory (packaging.json)")
    parser.add_argument("--discover-root", type=Path, help="hash-verified inventory only; never loads native code")
    parser.add_argument("--declaration-of", type=Path, metavar="LIBRARY",
                        help="print the sidecar plugin.toml a built artifact publishes (its embedded manifest)")
    parser.add_argument("--component", choices=["backend", "frontend"], default="backend")
    parser.add_argument("--feature", action="append", default=[])
    parser.add_argument("--artifact-id", help="Build label retained in artifact bytes; defaults to a fresh UUID; byte hash is artifact identity")
    parser.add_argument("--build", action="store_true")
    parser.add_argument("--release", action="store_true")
    parser.add_argument("--target")
    parser.add_argument("--cargo-target-dir", type=Path,
                        help="target directory dedicated to dylib builds; defaults to <target>/plugin-dylib of the crate's workspace")
    parser.add_argument("--install-root", type=Path,
                        help="publish the artifact with its embedded generated manifest as plugin.toml")
    parser.add_argument("--locked", action="store_true",
                        help="direct builds: require the crate's Cargo.lock to be up to date")
    args = parser.parse_args()
    if args.declaration_of:
        print(declaration_of(args.declaration_of.resolve()), end="")
        return
    if args.discover_root:
        if args.build or args.install_root:
            parser.error("discovery cannot build or install")
        print(json.dumps(discover(args.discover_root.resolve()), indent=2))
        return
    if not args.implementation_manifest:
        parser.error("packaging requires --implementation-manifest")
    if args.cargo_target_dir and not args.build:
        parser.error("--cargo-target-dir requires --build")
    if args.install_root and not args.build:
        parser.error("--install-root requires --build")
    artifact_id = args.artifact_id or str(uuid.uuid4())
    implementation = args.implementation_manifest.resolve()
    if not packages_itself(tomllib.loads(implementation.read_text()), args.component):
        parser.error(f"{implementation} cannot be packaged as a {args.component}: declare the "
                     f"{ROLE_FEATURE.format(args.component)!r} feature and define the plugin with plugin!")
    if not args.build:
        parser.error("a plugin crate packages itself; there is nothing to generate without --build")
    library, provenance = build_direct(implementation, args.component, args.feature,
                                       args.release, args.target, args.cargo_target_dir,
                                       artifact_id, args.locked)
    if args.output:
        output = args.output.resolve()
        output.mkdir(parents=True, exist_ok=True)
        (output / "packaging.json").write_text(json.dumps(
            {"schema_version": 2, **provenance, "library": str(library)}, indent=2) + "\n")
    print(library)
    if args.install_root:
        print(install(library, args.install_root.resolve(), args.component, artifact_id))

if __name__ == "__main__":
    main()
