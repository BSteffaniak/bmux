#!/usr/bin/env python3
"""No-build release fixtures: inventory, Unix/Windows policy, hashes and slots."""
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

SCRIPT = Path(__file__).with_name("auxiliary-executables.py")
INSTALLER = Path(__file__).with_name("install-artifacts.py")


class ReleaseFixtures(unittest.TestCase):
    def test_stage_and_install(self):
        with tempfile.TemporaryDirectory(prefix="bmux-release-fixture-") as directory:
            root = Path(directory)
            manifest = root / "plugins/fixture/plugin.toml"
            manifest.parent.mkdir(parents=True)
            manifest.write_text('id = "fixture.owner"\n[[auxiliary_executables]]\n'
                'id = "helper"\ncargo_package = "fixture_package"\ncargo_binary = "fixture-helper"\n'
                'relative_path = "fixture-helper"\nprotocol_version = 1\n')
            artifacts = root / "build"
            artifacts.mkdir()
            for name in ["bmux", "fixture-helper", "bmux.exe"]:
                (artifacts / name).write_bytes(f"fixture:{name}".encode())
                (artifacts / name).chmod(0o755)
            stage = root / "stage"
            command = [sys.executable, str(SCRIPT), "stage", "--workspace", str(root),
                "--target", "x86_64-unknown-linux-gnu", "--artifact-dir", str(artifacts),
                "--destination", str(stage)]
            subprocess.run(command, check=True)
            inventory = json.loads((stage / "artifacts.json").read_text())
            self.assertEqual(len(inventory["artifacts"]), 3)
            self.assertEqual(inventory["artifacts"][2]["declaration"]["protocol_version"], 1)
            prefix = root / "prefix"
            subprocess.run([sys.executable, str(INSTALLER), str(stage), str(prefix),
                            "--activate-version", "test-version"], check=True)
            self.assertTrue((prefix / "bin/bmux").is_symlink())
            self.assertEqual((prefix / "bin/bmux").resolve().parent,
                             (prefix / "lib/bmux/versions/test-version").resolve())
            self.assertEqual((prefix / "bin/bmux").resolve().with_name("fixture-helper").read_bytes(),
                             b"fixture:fixture-helper")
            old_target = (prefix / "bin/bmux").resolve()
            subprocess.run([sys.executable, str(INSTALLER), str(stage), str(prefix),
                            "--activate-version", "second-version"], check=True)
            self.assertNotEqual((prefix / "bin/bmux").resolve(), old_target)
            self.assertEqual(old_target.read_bytes(), b"fixture:bmux")
            self.assertEqual(old_target.with_name("fixture-helper").read_bytes(), b"fixture:fixture-helper")
            slot = root / "slot"
            subprocess.run([sys.executable, str(INSTALLER), str(stage), str(slot),
                            "--binary-name", "bmux-test"], check=True)
            self.assertEqual((slot / "fixture-helper").read_bytes(), b"fixture:fixture-helper")
            self.assertEqual((slot / "bmux-test").read_bytes(), b"fixture:bmux")
            self.assertFalse((slot / "bmux").exists())
            (stage / "fixture-helper").write_bytes(b"corrupt")
            failed = subprocess.run([sys.executable, str(INSTALLER), str(stage), str(root / "bad")],
                                    capture_output=True)
            self.assertNotEqual(failed.returncode, 0)
            self.assertFalse((root / "bad").exists())
            windows = root / "windows"
            command[command.index("x86_64-unknown-linux-gnu")] = "x86_64-pc-windows-msvc"
            command[-1] = str(windows)
            subprocess.run(command, check=True)
            self.assertFalse((windows / "fixture-helper").exists())
            inventory = json.loads((windows / "artifacts.json").read_text())
            self.assertEqual(len(inventory["unavailable_auxiliary_executables"]), 1)
            config = root / "nfpm.yaml"
            config.write_text("contents:\n  - src: bmux\n    dst: /usr/bin/bmux\n")
            generated = root / "generated.yaml"
            subprocess.run([sys.executable, str(SCRIPT), "nfpm", "--workspace", str(root),
                "--target", "aarch64-unknown-linux-gnu", "--config", str(config),
                "--destination", str(generated)], check=True)
            self.assertIn("dst: /usr/bin/fixture-helper", generated.read_text())
            manifest.write_text(manifest.read_text().replace('relative_path = "fixture-helper"',
                                                            'relative_path = "../escape"'))
            failed = subprocess.run(command, capture_output=True)
            self.assertNotEqual(failed.returncode, 0)


if __name__ == "__main__":
    unittest.main()
