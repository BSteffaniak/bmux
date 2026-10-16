#!/usr/bin/env python3
"""Guard build/deploy vs validation separation without requiring a cloud account."""
from pathlib import Path
import hashlib
import os
import re
import shutil
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parent.parent


class DeploymentBoundaryTests(unittest.TestCase):
    def test_candidate_builder_contains_no_test_or_lint_suites(self):
        source = (ROOT / 'scripts/build-live-artifact.sh').read_text()
        for command in (r'cargo\s+(?:clippy|nextest|machete|test|check)\b', r'npm\s+test\b',
                        r'python3\s+scripts/test-', r'(?:bash|python3)\s+scripts/(?:smoke-|compat-matrix)'):
            self.assertIsNone(re.search(command, source), command)
        for required in ('cargo build --locked --release', 'SHA256SUMS', 'git rev-parse HEAD',
                         'plugin rebuild --all-workspace-plugins'):
            self.assertIn(required, source)

    def test_release_workflow_does_not_run_validation(self):
        source = (ROOT / '.github/workflows/live-promote.yml').read_text()
        for forbidden in ('scripts/test-', 'cargo clippy', 'cargo nextest', 'Offline infrastructure validation',
                          'fmt -check', 'tofu -chdir=infra/live-terraform validate'):
            self.assertNotIn(forbidden, source)
        for required in ('environment: production-approval', 'environment: Production', 'cancel-in-progress: false',
                         'sha256sum -c live-candidate.tar.gz.sha256', 'Bootstrap completion checksum mismatch',
                         'scripts/live-deploy.sh'):
            self.assertIn(required, source)

    def test_site_and_backend_have_separate_publishers(self):
        workflow = (ROOT / '.github/workflows/docs-deploy.yml').read_text()
        for forbidden in ('live-promote.yml', 'live-deploy.sh', 'R2_STATE_', 'TOFU_', 'setup-opentofu'):
            self.assertNotIn(forbidden, workflow)
        for required in ('build-site-artifact.sh', 'pages deploy site', '--component site',
                         'bmux-production-pages', 'sha256sum -c SHA256SUMS',
                         'environment: production-approval', 'environment: Production'):
            self.assertIn(required, workflow)
        builder = (ROOT / 'scripts/build-site-artifact.sh').read_text()
        self.assertLess(builder.index('build-web-renderer.sh\n'), builder.index('cargo run --locked'))
        for forbidden in ('bmux_cli', 'plugin rebuild', 'tofu ', 'live-deploy.sh'):
            self.assertNotIn(forbidden, builder)
        for file in ('scripts/live-deploy.sh', 'scripts/live-install.py'):
            source = (ROOT / file).read_text()
            self.assertNotIn('pages deploy', source)
            self.assertNotIn("'pages', 'deploy'", source)
        backend = (ROOT / 'scripts/build-live-artifact.sh').read_text()
        self.assertNotIn('build-web-renderer.sh', backend)
        self.assertNotIn('bmux_docs_site', backend)
        release = (ROOT / '.github/workflows/live-promote.yml').read_text()
        self.assertIn('  push:', release)
        self.assertNotIn('packages/docs-site/**', release)

    def test_site_builder_packages_complete_site_without_backend_tools(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            tools = root / 'bin'
            tools.mkdir()
            # Fail on unexpected build invocations; no real compilation/npm/network.
            bash = shutil.which('bash')
            self.assertIsNotNone(bash)
            commands = {
                'bash': 'test "$1" = scripts/build-web-renderer.sh\nmkdir -p "$BMUX_WEB_ASSET_OUTPUT"\necho viewer > "$BMUX_WEB_ASSET_OUTPUT/live-page.js"',
                'cargo': 'test "$*" = "run --locked --release -p bmux_docs_site -- gen --output $9"\nmkdir -p "$9"\necho \'<title>MoosicBox</title><meta name="description" content="MoosicBox: A music app for cows">\' > "$9/index.html"\ncp "$(dirname "$9")/viewer/live-page.js" "$9/live-page.js"',
                'npm': 'test "$*" = "ci --ignore-scripts --no-audit --no-fund"\nmkdir -p node_modules/wrangler/bin\necho pinned-tool > node_modules/wrangler/bin/wrangler.js',
            }
            for name, body in commands.items():
                path = tools / name
                path.write_text('#!' + bash + '\nset -euo pipefail\n' + body + '\n')
                path.chmod(0o755)
            output = root / 'candidate'
            env = dict(os.environ, BMUX_NIX_TOOLCHAIN='test', PATH=str(tools) + os.pathsep + os.environ['PATH'])
            result = subprocess.run([bash, str(ROOT / 'scripts/build-site-artifact.sh'), str(output)],
                                    env=env, capture_output=True, text=True, check=False)
            self.assertEqual(result.returncode, 0, result.stderr)
            for name in ('site/index.html', 'site/live-page.js', 'REVISION',
                         'tools/node_modules/wrangler/bin/wrangler.js'):
                self.assertTrue((output / name).is_file(), name)
            html = (output / 'site/index.html').read_text()
            self.assertIn('<title>bmux — Live broadcasts and documentation</title>', html)
            self.assertNotIn('MoosicBox', html)
            self.assertFalse((output / 'relay').exists())
            self.assertFalse((output / 'native').exists())
            for line in (output / 'SHA256SUMS').read_text().splitlines():
                digest, name = line.split('  ', 1)
                self.assertEqual(hashlib.sha256((output / name).read_bytes()).hexdigest(), digest)

    def test_live_tests_and_acceptance_remain_in_independent_ci(self):
        workflow = (ROOT / '.github/workflows/live-validation.yml').read_text()
        for name in ('test-live-bootstrap.py', 'test-live-github-environments.py', 'test-setup-live.py',
                     'test-live-state-recovery.py', 'test-live-state-provision.py', 'test-live-imports.py',
                     'test-live-plan-policy.py', 'test-live-deploy.py', 'test-live-install.py', 'test-live-state.py',
                     'test-archive-live-state.py', 'test-live-smoke.py', 'test-live-deployment-boundary.py',
                     'npm test', 'smoke-pty-runtime.sh', 'compat-matrix.sh', 'smoke-live-local.py',
                     'fmt -check', 'init -backend=false', 'infra/live-terraform validate'):
            self.assertIn(name, workflow)
        self.assertNotIn('secrets.', workflow)
        self.assertNotIn('needs:', workflow)  # Validation cannot depend on a deployment/approval.


if __name__ == '__main__':
    unittest.main()
