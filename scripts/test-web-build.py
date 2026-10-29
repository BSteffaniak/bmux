#!/usr/bin/env python3
"""Exercise web asset builds across dependency upgrades, without downloading tools."""
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parent.parent


class WebBuildTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="bmux-web-build-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        for name in ("scripts/build-web-renderer.sh", "scripts/presenter-adapter.js"):
            target = self.root / name
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(ROOT / name, target)
        shutil.copytree(ROOT / "packages/web-renderer/browser",
                        self.root / "packages/web-renderer/browser")
        self.bin = self.root / "bin"
        self.bin.mkdir()
        self.tool_source = self.bin / "bindgen-template"
        self.tool_source.write_text('''#!/usr/bin/env python3
import os, pathlib, sys
args = sys.argv[1:]
if args == ['--version']:
    print('wasm-bindgen ' + pathlib.Path(__file__).with_suffix('.version').read_text())
    sys.exit(0)
if os.environ.get('FAIL_CONVERSION'):
    sys.exit('incompatible Wasm schema')
out = pathlib.Path(args[args.index('--out-dir') + 1])
name = args[args.index('--out-name') + 1]
(out / (name + '.js')).write_text('export default async function init() {}')
(out / (name + '_bg.wasm')).write_bytes(b'\\0asm\\1\\0\\0\\0')
''')
        cargo = self.bin / "cargo"
        cargo.write_text('''#!/usr/bin/env python3
import json, os, pathlib, shutil, sys
args = sys.argv[1:]
if args[0] == 'tree':
    print('wasm-bindgen v' + os.environ['RESOLVED_VERSION'] if 'wasm-bindgen' in args else 'bmux_presenter v1.0.0')
elif args[0] == 'install':
    root = pathlib.Path(args[args.index('--root') + 1]) / 'bin'
    root.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(os.environ['TOOL_SOURCE'], root / 'wasm-bindgen')
    (root / 'wasm-bindgen').chmod(0o755)
    (root / 'wasm-bindgen.version').write_text(args[args.index('--version') + 1])
    with open(os.environ['INSTALL_LOG'], 'a') as log:
        log.write(json.dumps(args) + '\\n')
elif args[0] != 'build':
    sys.exit('unexpected cargo command')
''')
        cargo.chmod(0o755)
        self.env = {**os.environ, "PATH": str(self.bin) + os.pathsep + os.environ["PATH"],
                    "BMUX_NIX_TOOLCHAIN": "test", "CARGO_TARGET_DIR": str(self.root / "target"),
                    "BMUX_WEB_TOOLS_ROOT": str(self.root / "tools"),
                    "BMUX_WEB_ASSET_OUTPUT": str(self.root / "assets"),
                    "BMUX_LIVE_ENDPOINT_ORIGIN": "https://relay.example.test",
                    "INSTALL_LOG": str(self.root / "installs"), "TOOL_SOURCE": str(self.tool_source)}
        self.env.pop("BMUX_WASM_BINDGEN", None)

    def build(self, version, **extra):
        return subprocess.run(["bash", str(self.root / "scripts/build-web-renderer.sh")],
                              env={**self.env, "RESOLVED_VERSION": version, **extra},
                              capture_output=True, text=True, check=False)

    def test_dependency_upgrade_builds_and_reuses_compatible_tools(self):
        for version in ("0.2.777", "0.2.778", "0.2.777"):
            result = self.build(version)
            self.assertEqual(result.returncode, 0, result.stderr)
            public = self.root / "packages/docs-site/public/live-assets"
            bundle = public / (public / "current-version").read_text().strip()
            manifest = json.loads((bundle / "manifest.json").read_text())
            self.assertTrue(manifest["files"])
            self.assertIn('https://relay.example.test/', (bundle / 'live-page.js').read_text())
            self.assertEqual((bundle / 'bmux_web_renderer_bg.wasm').read_bytes()[:4], b'\0asm')
        installs = [json.loads(line) for line in (self.root / "installs").read_text().splitlines()]
        self.assertEqual([args[args.index('--version') + 1] for args in installs],
                         ['0.2.777', '0.2.778'])

    def test_override_is_accepted_by_conversion_not_version_label(self):
        override = self.bin / "custom-bindgen"
        shutil.copyfile(self.tool_source, override)
        override.chmod(0o755)
        # No version file: querying --version would fail. Successful conversion
        # is sufficient for a caller-supplied compatible implementation.
        result = self.build("0.2.777", BMUX_WASM_BINDGEN=str(override))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse((self.root / "installs").exists())

    def test_conversion_failure_never_publishes_bundle(self):
        result = self.build("0.2.777", FAIL_CONVERSION="1")
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse((self.root / "packages/docs-site/public/live-assets/current-version").exists())


if __name__ == '__main__':
    unittest.main()
