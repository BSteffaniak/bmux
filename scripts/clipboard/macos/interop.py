"""Run with python3 from the macOS repo Nix shell. Never uses the general board."""

import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tarfile
import tempfile
import uuid

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[2]
CHILD = "native::macos::tests::named_board_child"
PINS = {
    "3.7.0": "aeb9553e62312c8756a5979254fc6d7da8bbeb5986a1e17037fab932f7037917",
    "3.6.1": "0348a1c054491f4bfe6ab86a7b6ab1e44e45d899005de92f58b3df180b36ddaf",
}


def build(args, cwd, target):
    output = subprocess.check_output(["cargo", *args, "--message-format=json-render-diagnostics"], cwd=cwd, text=True)
    return next(m["executable"] for line in output.splitlines() if (m := json.loads(line)).get("executable") and m["target"]["name"] == target)


def reference(tmp, version, token):
    # Verify and extract the original archive, never edit the registry source.
    cache = Path(os.getenv("CARGO_HOME", Path.home() / ".cargo")) / "registry/cache"
    archive = next(cache.glob(f"*/arboard-{version}.crate"))
    if hashlib.sha256(archive.read_bytes()).hexdigest() != PINS[version]:
        raise ValueError(f"unrecognized arboard {version} archive")
    with tarfile.open(archive) as tar:
        tar.extractall(tmp, filter="data")
    src = tmp / f"arboard-{version}/src/platform/osx.rs"
    text = src.read_text()
    old = 'msg_send![NSPasteboard::class(), generalPasteboard]'
    new = f'msg_send![NSPasteboard::class(), pasteboardWithName: ns_string!("org.bmux.clipboard.test.{token}")]'
    if text.count(old) != 1:
        raise ValueError("expected exactly one board constructor")
    src.write_text(text.replace(old, new))
    (tmp / "src").mkdir()
    shutil.copy(HERE / "baseline.rs", tmp / "src/main.rs")
    (tmp / "Cargo.toml").write_text(f'''[package]
name = "bmux-mac-interop-{token}"
version = "0.0.0"
edition = "2024"
[dependencies]
arboard = {{ path = "arboard-{version}", default-features = false, features = ["image-data"] }}
''')
    shutil.copy(ROOT / "Cargo.lock", tmp / "Cargo.lock")
    return build(["build", "--offline"], tmp, f"bmux-mac-interop-{token}")


def main():
    if sys.platform != "darwin" or not os.getenv("IN_NIX_SHELL"):
        sys.exit("requires macOS in the repository Nix shell")
    native = build(["test", "--locked", "-p", "bmux_clipboard", "--features", "native-clipboard", "--lib", "--no-run"], ROOT, "bmux_clipboard")
    if CHILD + ": test" not in subprocess.check_output([native, "--list", "--ignored"], text=True).splitlines():
        sys.exit("missing named-board child")
    with tempfile.TemporaryDirectory() as directory:
        tmp = Path(directory)
        peer = tmp / "peer"
        subprocess.run(["clang", "-fobjc-arc", "-Wall", "-Werror", HERE / "peer.m", "-framework", "AppKit", "-o", peer], check=True)
        for backend in ["native", *PINS]:
            token = uuid.uuid4().hex
            if backend != "native":
                work = tmp / backend
                work.mkdir()
                exe = reference(work, backend, token)

            def appkit(op):
                subprocess.run([peer, token, op], check=True, timeout=15)

            def rust(op):
                if backend == "native":
                    subprocess.run([native, "--ignored", "--exact", CHILD], env={**os.environ, "BMUX_MAC_INTEROP_TOKEN": token, "BMUX_MAC_INTEROP_OP": op}, check=True, timeout=15)
                else:
                    subprocess.run([exe, op], check=True, timeout=15)

            try:
                for fmt in ["text", "tiff"] + (["png"] if backend == "native" else []):
                    kind = "text" if fmt == "text" else "image"
                    for writer, reader, src, dst in [(appkit, rust, fmt, kind), (rust, appkit, kind, fmt)]:
                        appkit("release")  # No stale fixture can satisfy this case.
                        writer("write-" + src)  # wait for process exit before reading
                        reader("read-" + dst)
                        print(f"PASS {backend}: {writer.__name__} {src} -> {reader.__name__} {dst} after exit", flush=True)
            finally:
                appkit("release")


if __name__ == "__main__":
    main()
