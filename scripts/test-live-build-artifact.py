#!/usr/bin/env python3
"""Exercise candidate packaging with fake build tools; no compiler or registry."""
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]


class BuildTests(unittest.TestCase):
    def test_nix_release_rootfs_oci_and_inventory(self):
        with tempfile.TemporaryDirectory() as tmp:
            work = Path(tmp)
            tools = work / 'bin'
            tools.mkdir()
            target = work / 'pinned-target'
            (target / 'release').mkdir(parents=True)
            executable = target / 'release/bmux'
            executable.write_text('#!/bin/sh\nexit 0\n')
            executable.chmod(0o700)
            (target / 'release/bmux-live-relay').write_bytes(b'owned-release-executable')
            log = work / 'effects'
            def tool(name, source):
                path = tools / name
                path.write_text('#!' + sys.executable + '\n' + source)
                path.chmod(0o700)
            tool('cargo', '''import json, os, sys
from pathlib import Path
with open(os.environ['TEST_EFFECTS'], 'a') as f: f.write(json.dumps(['cargo', *sys.argv[1:]]) + '\\n')
if sys.argv[1] == 'metadata': print(json.dumps({'target_directory': os.environ['CARGO_TARGET_DIR']}))
''')
            tool('bash', '''import json, os, sys
from pathlib import Path
assert sys.argv[1] == 'packages/live-relay/package-rootfs.sh'
assert sys.argv[2] == os.environ['CARGO_TARGET_DIR'] + '/release/bmux-live-relay'
root = Path(sys.argv[3])
assert root.name == 'live-relay-rootfs' and root.parent.name == 'artifacts'
(root / 'bin').mkdir(parents=True)
(root / 'bin/bmux-live-relay').write_bytes(Path(sys.argv[2]).read_bytes())
with open(os.environ['TEST_EFFECTS'], 'a') as f: f.write(json.dumps(['rootfs', *sys.argv[2:]]) + '\\n')
''')
            tool('docker', '''import json, os, sys
from pathlib import Path
args = sys.argv[1:]
assert args[:2] == ['buildx', 'build']
assert '--provenance=false' in args
assert args[args.index('--platform') + 1] == 'linux/amd64'
context = Path(args[-1])
assert (context / 'artifacts/live-relay-rootfs/bin/bmux-live-relay').read_bytes() == b'owned-release-executable'
assert Path(args[args.index('--file') + 1]).read_bytes() == Path('packages/live-relay/Dockerfile').read_bytes()
output = args[args.index('--output') + 1]
assert output.startswith('type=oci,dest=')
Path(output.removeprefix('type=oci,dest=')).write_bytes(b'mock-immutable-oci')
Path(args[args.index('--metadata-file') + 1]).write_text(json.dumps({'containerimage.digest': 'sha256:' + 'd' * 64}))
with open(os.environ['TEST_EFFECTS'], 'a') as f: f.write(json.dumps(['docker', *args]) + '\\n')
''')
            tool('npm', 'import sys\nassert sys.argv[1:] == ["ci", "--ignore-scripts", "--no-audit", "--no-fund"]\n')
            env = dict(os.environ, PATH=str(tools) + os.pathsep + os.environ['PATH'],
                       BMUX_NIX_TOOLCHAIN='1', CARGO_TARGET_DIR=str(target),
                       LIVE_FLY_APP='bmux-live-test', TEST_EFFECTS=str(log), TMPDIR=str(work))
            output = work / 'candidate'
            result = subprocess.run([shutil.which('bash'), str(ROOT / 'scripts/build-live-artifact.sh'), str(output)],
                                    env=env, capture_output=True, text=True, timeout=30)
            self.assertEqual(result.returncode, 0, result.stderr)
            effects = [json.loads(line) for line in log.read_text().splitlines()]
            self.assertIn(['cargo', 'build', '--locked', '--release', '-p', 'bmux-live-relay'], effects)
            self.assertTrue(any(row[:2] == ['cargo', 'metadata'] for row in effects))
            self.assertEqual((output / 'relay/IMAGE').read_text(), 'registry.fly.io/bmux-live-test@sha256:' + 'd' * 64 + '\n')
            self.assertTrue((output / 'relay/image.oci.tar').is_file())
            self.assertFalse((output / 'relay/worker.js').exists())
            self.assertTrue((output / 'infrastructure/fly.tf').is_file())
            for line in (output / 'SHA256SUMS').read_text().splitlines():
                digest, name = line.split('  ', 1)
                self.assertEqual(hashlib.sha256((output / name).read_bytes()).hexdigest(), digest)
            self.assertFalse(any('push' in row or 'apply' in row for row in effects))


if __name__ == '__main__': unittest.main()
