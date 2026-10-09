#!/usr/bin/env python3
"""Exercise rendered Worker installer using local fixtures; no network/cloud calls."""
import hashlib
import io
import json
from pathlib import Path
import subprocess
import tarfile
import tempfile
import unittest
import os

WORKSPACE = Path(__file__).resolve().parents[2]


class InstallerFixtures(unittest.TestCase):
    def test_renderer_and_safe_activation(self):
        with tempfile.TemporaryDirectory(prefix="bmux-installer-fixture-") as directory:
            root = Path(directory)
            module = root / "worker.mjs"
            module.write_text((WORKSPACE / "infra/wrangler/src/index.js").read_text())
            rendered = subprocess.check_output(["node", "--input-type=module", "-e",
                f'import {{installScript}} from {json.dumps(module.as_uri())}; console.log(installScript("fixture.invalid","stable","1.2.3"));'])
            script = root / "install.sh"
            script.write_bytes(rendered)
            subprocess.run(["sh", "-n", str(script)], check=True)
            target = "x86_64-unknown-linux-gnu"
            filename = f"bmux-1.2.3-{target}.tar.gz"
            fixture = root / "downloads"
            fixture.mkdir()
            commands = root / "commands"
            commands.mkdir()
            (commands / "uname").write_text('#!/bin/sh\ncase "$1" in -s) echo Linux;; -m) echo x86_64;; esac\n')
            (commands / "curl").write_text('#!/bin/sh\nfor arg do case "$arg" in https:*) url="$arg";; esac; done\nwhile [ "$1" != -o ]; do shift; done\ncp "$FIXTURE_DOWNLOADS/${url##*/}" "$2"\n')
            for command in commands.iterdir():
                command.chmod(0o755)
            environment = {**os.environ, "PATH": str(commands) + os.pathsep + os.environ["PATH"],
                "FIXTURE_DOWNLOADS": str(fixture), "BMUX_INSTALL_PREFIX": str(root / "prefix")}
            payload = {"bmux": b"fixture-cli", "fixture-helper": b"fixture-helper",
                "install-artifacts.py": (WORKSPACE / "scripts/release/install-artifacts.py").read_bytes()}
            inventory = {"schema_version": 1, "target": target, "artifacts": [
                {"relative_path": name, "bytes": len(data), "sha256": hashlib.sha256(data).hexdigest()}
                for name, data in payload.items()]}
            payload["artifacts.json"] = json.dumps(inventory).encode()

            def archive(extra=None):
                with tarfile.open(fixture / filename, "w:gz") as tar:
                    for name, data in payload.items():
                        member = tarfile.TarInfo("./" + name)
                        member.size = len(data)
                        tar.addfile(member, io.BytesIO(data))
                    if extra:
                        tar.addfile(extra, io.BytesIO(b"x") if extra.isfile() else None)
                digest = hashlib.sha256((fixture / filename).read_bytes()).hexdigest()
                (fixture / "SHA256SUMS").write_text(f"{digest}  {filename}\n")

            archive()
            subprocess.run(["sh", str(script)], env=environment, check=True, capture_output=True)
            link = root / "prefix/bin/bmux"
            original = link.resolve()
            self.assertEqual(original.with_name("fixture-helper").read_bytes(), b"fixture-helper")
            archive()
            (fixture / "SHA256SUMS").write_text(f"{'0' * 64}  {filename}\n")
            failed = subprocess.run(["sh", str(script)], env=environment, capture_output=True)
            self.assertNotEqual(failed.returncode, 0)
            self.assertIn(b"Archive checksum mismatch", failed.stderr)
            self.assertEqual(link.resolve(), original)
            escape = tarfile.TarInfo("../escape")
            escape.size = 1
            archive(escape)
            failed = subprocess.run(["sh", str(script)], env=environment, capture_output=True)
            self.assertNotEqual(failed.returncode, 0)
            self.assertEqual(link.resolve(), original)
            self.assertFalse((root / "escape").exists())
            symlink = tarfile.TarInfo("link")
            symlink.type = tarfile.SYMTYPE
            symlink.linkname = "/tmp"
            archive(symlink)
            self.assertNotEqual(subprocess.run(["sh", str(script)], env=environment,
                                              capture_output=True).returncode, 0)
            payload.pop("artifacts.json")
            archive()
            failed = subprocess.run(["sh", str(script)], env=environment, capture_output=True)
            self.assertNotEqual(failed.returncode, 0)
            self.assertIn(b"Legacy inventory-less archive refused", failed.stderr)
            self.assertEqual(link.resolve(), original)


if __name__ == "__main__":
    unittest.main()
