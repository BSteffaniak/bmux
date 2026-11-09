#!/usr/bin/env python3
"""Inspect actual native plugin artifacts through negotiated ABI v2/v3, never source text.

Trusted native libraries execute in this process. Run only coordinator-built
artifacts in an isolated integration environment. Discovery must remain inert;
this harness never creates or activates a plugin, installs, or contacts a daemon.

Example (after source convergence and artifact builds):
  python3 scripts/shared-plugin-artifact-acceptance.py \
    --artifact bcode.file-references=/absolute/libfile_references.dylib \
    --artifact bcode.filesystem=/absolute/libfilesystem.dylib \
    --samples 100 --report /absolute/artifact-acceptance.json

An expected identity is supplied independently of the artifact. This catches
file-references embedding filesystem's manifest even when its filename is right.
The report records metadata-call cost, not invocation/render performance.
"""
import argparse
import ctypes
import hashlib
import json
import pathlib
import statistics
import time

MAX_BYTES = 1024 * 1024
OK = 0
BUFFER_TOO_SMALL = 3
# ABI-v2 scoped ports: events=1, host services=2, live cancellation=4,
# activation registrations=8. These bits describe support, not authorization.
# Keep in sync with SUPPORTED_CAPABILITIES_BY_ABI in package-plugin.py: v3 adds
# profiles up to callback authority (2047) and host wake v1 (4095, bit 2048),
# which requires the size-checked `read_wake` descriptor tail.
SCOPED_CAPABILITIES = {2: frozenset({15}), 3: frozenset({31, 127, 255, 511, 1023, 2047, 4095})}
CAP_HOST_WAKE_V1 = 2048

MetadataFn = ctypes.CFUNCTYPE(ctypes.c_int32, ctypes.c_void_p, ctypes.c_size_t,
                             ctypes.POINTER(ctypes.c_size_t))


class DescriptorHeader(ctypes.Structure):
    _fields_ = [("size", ctypes.c_uint32), ("version", ctypes.c_uint32)]


class InvocationPorts(ctypes.Structure):
    # Ports version 2. This metadata-only harness never supplies executable ports.
    _fields_ = [("size", ctypes.c_uint32), ("version", ctypes.c_uint32),
                ("capabilities", ctypes.c_uint64), ("handle", ctypes.c_uint64),
                ("retain", ctypes.c_void_p), ("release", ctypes.c_void_p),
                ("emit_event", ctypes.c_void_p), ("host_begin", ctypes.c_void_p),
                ("host_read", ctypes.c_void_p), ("host_ack", ctypes.c_void_p),
                ("registration_begin", ctypes.c_void_p)]


PORTS_VERSION = 2


class Descriptor(ctypes.Structure):
    # Function pointers not exercised here remain opaque. No Rust layout crosses ABI.
    _fields_ = [("size", ctypes.c_uint32), ("version", ctypes.c_uint32),
                ("capabilities", ctypes.c_uint64), ("start_scoped", ctypes.c_void_p),
                ("metadata", MetadataFn),
                ("create", ctypes.c_void_p), ("destroy", ctypes.c_void_p),
                ("start", ctypes.c_void_p), ("read", ctypes.c_void_p),
                ("cancel", ctypes.c_void_p), ("ack", ctypes.c_void_p)]


class DescriptorV3(ctypes.Structure):
    _fields_ = [("base", Descriptor), ("declarations", ctypes.c_void_p)]


class DescriptorV3HostWake(ctypes.Structure):
    _fields_ = [("base", DescriptorV3), ("read_wake", ctypes.c_void_p)]


def read_metadata(descriptor):
    required = ctypes.c_size_t(0)
    status = descriptor.metadata(None, 0, ctypes.byref(required))
    if status != BUFFER_TOO_SMALL or not 0 < required.value <= MAX_BYTES:
        raise ValueError(f"invalid metadata size response: status={status}, size={required.value}")
    output = ctypes.create_string_buffer(required.value)
    capacity = required.value
    status = descriptor.metadata(output, capacity, ctypes.byref(required))
    if status != OK or not 0 < required.value <= capacity:
        raise ValueError(f"invalid metadata retrieval: status={status}, size={required.value}")
    value = json.loads(output.raw[:required.value])
    if not isinstance(value, dict):
        raise ValueError("metadata must be an object")
    return value


def inspect_artifact(expected, path, samples):
    path = pathlib.Path(path).resolve(strict=True)
    if not path.is_file():
        raise ValueError(f"not a regular artifact: {path}")
    before = hashlib.sha256(path.read_bytes()).hexdigest()
    # Native code is trusted by explicit artifact selection. No search-path fallback.
    library = ctypes.CDLL(str(path))
    entry = getattr(library, "bmux_plugin_entry_v2")
    entry.argtypes = []
    entry.restype = ctypes.c_void_p
    pointer = entry()
    if not pointer:
        raise ValueError("entry returned a null descriptor")
    header = ctypes.cast(pointer, ctypes.POINTER(DescriptorHeader)).contents
    expected_capabilities = SCOPED_CAPABILITIES.get(header.version)
    minimum_size = ctypes.sizeof(DescriptorV3 if header.version == 3 else Descriptor)
    if expected_capabilities is None or header.size < minimum_size:
        raise ValueError(f"unsupported descriptor: version={header.version}, size={header.size}")
    descriptor = ctypes.cast(pointer, ctypes.POINTER(Descriptor)).contents
    if not descriptor.metadata:
        raise ValueError("descriptor has no metadata callback")
    if descriptor.capabilities not in expected_capabilities:
        raise ValueError("native capabilities do not match the negotiated ABI")
    if descriptor.capabilities & CAP_HOST_WAKE_V1 and (
            header.size < ctypes.sizeof(DescriptorV3HostWake)
            or not ctypes.cast(pointer, ctypes.POINTER(DescriptorV3HostWake)).contents.read_wake):
        raise ValueError("host wake capability requires the complete read_wake descriptor tail")
    if header.version == 3 and not ctypes.cast(pointer, ctypes.POINTER(DescriptorV3)).contents.declarations:
        raise ValueError("v3 descriptor has no declarations callback")
    if not all(getattr(descriptor, field) for field in
               ("start_scoped", "create", "destroy", "start", "read", "cancel", "ack")):
        raise ValueError("incomplete shared ABI operation table")
    metadata = read_metadata(descriptor)
    if metadata.get("plugin_id") != expected:
        raise ValueError(f"embedded identity mismatch: expected {expected!r}, got {metadata.get('plugin_id')!r}")
    if not metadata.get("version") or not metadata.get("roles"):
        raise ValueError("metadata must declare a version and component roles")
    manifest = metadata.get("manifest_toml")
    if manifest is not None:
        import tomllib
        declaration = tomllib.loads(manifest)
        if declaration.get("id") != expected:
            raise ValueError(f"embedded manifest identity mismatch for {expected}")
    timings = []
    for _ in range(samples):
        started = time.perf_counter_ns()
        observed = read_metadata(descriptor)
        timings.append(time.perf_counter_ns() - started)
        if observed != metadata:
            raise ValueError("metadata changed across inert discovery calls")
    after = hashlib.sha256(path.read_bytes()).hexdigest()
    if before != after:
        raise ValueError("artifact changed during acceptance; rebuild and retry explicitly")
    return {"expected_id": expected, "path": str(path), "sha256": before,
            "abi_version": header.version, "capabilities": descriptor.capabilities,
            "metadata": metadata, "samples": samples,
            "metadata_roundtrip_median_ns": statistics.median(timings),
            "metadata_roundtrip_max_ns": max(timings)}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--artifact", action="append", required=True, metavar="EXPECTED_ID=PATH")
    parser.add_argument("--samples", type=int, default=20)
    parser.add_argument("--report", type=pathlib.Path, required=True)
    args = parser.parse_args()
    if not 1 <= args.samples <= 10000:
        parser.error("samples must be between 1 and 10000")
    artifacts = []
    for item in args.artifact:
        expected, separator, path = item.partition("=")
        if not expected or not separator or not pathlib.Path(path).is_absolute():
            parser.error("artifact requires EXPECTED_ID=/absolute/path")
        artifacts.append((expected, path))
    results = []
    failures = []
    for expected, path in artifacts:
        try:
            results.append(inspect_artifact(expected, path, args.samples))
        except (AttributeError, OSError, ValueError) as error:
            failures.append({"expected_id": expected, "path": path, "error": str(error)})
    report = {"schema": "shared-plugin-artifact-acceptance.v1",
              "scope": "native metadata identity only; not full packaging/behavior parity",
              "results": results, "failures": failures}
    # Exclusive creation prevents overwriting integration evidence accidentally.
    with args.report.open("x", encoding="utf-8") as output:
        json.dump(report, output, indent=2)
        output.write("\n")
    if failures:
        raise SystemExit(1)


if __name__ == "__main__":
    main()
