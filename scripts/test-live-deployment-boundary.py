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
    def test_runtime_smoke_builds_static_plugin_auxiliary_executables(self):
        # Clean CI has no helper left by cargo test/plugin rebuild. Both entry
        # points must build it beside bmux before attempting a real terminal.
        for name in ('smoke-pty-runtime.sh', 'compat-matrix.sh'):
            source = (ROOT / 'scripts' / name).read_text()
            self.assertIn('cargo build -q --locked -p bmux_cli '
                          '-p bmux_pane_runtime_plugin --bins', source)
        matrix = (ROOT / 'scripts/compat-matrix.sh').read_text()
        self.assertIn('if [[ "$EXPLICIT_BINARY" == "0" ]] && ! cargo build', matrix)

    def test_candidate_builder_contains_no_test_or_lint_suites(self):
        source = (ROOT / 'scripts/build-live-artifact.sh').read_text()
        for command in (r'cargo\s+(?:clippy|nextest|machete|test|check)\b', r'npm\s+test\b',
                        r'python3\s+scripts/test-', r'(?:bash|python3)\s+scripts/(?:smoke-|compat-matrix)'):
            self.assertIsNone(re.search(command, source), command)
        for required in ('cargo build --locked --release', 'SHA256SUMS', 'git rev-parse HEAD',
                         'plugin rebuild --all-workspace-plugins'):
            self.assertIn(required, source)

    def test_configure_caller_grants_nested_read_permissions_without_write_authority(self):
        caller = (ROOT / '.github/workflows/configure-production-infra.yml').read_text()
        permissions = caller.split('\npermissions:\n', 1)[1].split('\njobs:', 1)[0]
        granted = dict(re.findall(r'^  ([a-z-]+): (read|write|none)\b', permissions, re.MULTILINE))
        self.assertEqual(granted, {'contents': 'read', 'actions': 'read'})
        for name in ('live-promote.yml', 'live-validation.yml', 'docs-deploy.yml'):
            nested = (ROOT / '.github/workflows' / name).read_text()
            for scope, access in re.findall(r'^ +([a-z-]+): (read|write|none)\b', nested, re.MULTILINE):
                if access != 'none':
                    self.assertEqual(granted.get(scope), access, f'{name}: {scope}')
        self.assertIn('configure: true', caller)
        self.assertIn('approve_import: ${{ inputs.approve_import }}', caller)

    def test_release_workflow_does_not_run_validation(self):
        source = (ROOT / '.github/workflows/live-promote.yml').read_text()
        for forbidden in ('scripts/test-', 'cargo clippy', 'cargo nextest', 'Offline infrastructure validation',
                          'fmt -check', 'tofu -chdir=infra/live-terraform validate'):
            self.assertNotIn(forbidden, source)
        for required in ('environment: production-approval', 'environment: Production', 'cancel-in-progress: false',
                         'sha256sum -c live-candidate.tar.gz.sha256', 'Bootstrap completion checksum mismatch',
                         'scripts/live-deploy.sh'):
            self.assertIn(required, source)

    def test_master_pushes_have_no_path_filter_and_apply_consumes_preview_receipt(self):
        release = (ROOT / '.github/workflows/live-promote.yml').read_text()
        push = release.split('  push:\n', 1)[1].split('  workflow_dispatch:', 1)[0]
        self.assertIn('branches: [master]', push)
        self.assertNotIn('paths', push)
        self.assertNotIn('\n  push:', (ROOT / '.github/workflows/docs-deploy.yml').read_text())
        self.assertNotIn('\n  push:', (ROOT / '.github/workflows/live-validation.yml').read_text())
        self.assertIn('  workflow_call:', (ROOT / '.github/workflows/live-validation.yml').read_text())
        self.assertIn('plan_receipt: ${{ steps.preview.outputs.plan_receipt }}', release)
        self.assertIn('LIVE_APPROVED_PLAN: ${{ needs.preview.outputs.plan_receipt }}', release)
        self.assertIn('needs: [build, approve, validation, preview]', release)
        self.assertIn('mode=approved', release)
        preview = release.split('  preview:\n', 1)[1].split('  approve:\n', 1)[0]
        self.assertNotIn('tofu apply', preview)
        self.assertNotIn('--configure', preview)

    def test_steady_state_release_keeps_preview_and_acceptance_gates(self):
        release = (ROOT / '.github/workflows/live-promote.yml').read_text()
        validation = (ROOT / '.github/workflows/live-validation.yml').read_text()
        for obsolete in ('reuse_backend', 'inputs.migrate', 'retire_worker',
                         'migration-summary', 'skip_acceptance', '39048863096'):
            self.assertNotIn(obsolete, release + validation)
        self.assertIn('needs.preview.result == \'success\'', release)
        self.assertIn('needs.validation.result == \'success\'', release)
        preview = release.split('  preview:\n')[1].split('  approve:\n')[0]
        self.assertIn('live-candidate/REVISION', preview)
        self.assertIn('live-candidate" preview', preview)
        self.assertNotIn('skopeo', preview)
        for name in ('live-inspect.yml', 'live-recover.yml', 'live-recovery-task.yml',
                     'live-repair-app.yml'):
            self.assertFalse((ROOT / '.github/workflows' / name).exists())

    def test_reusable_validation_has_no_deploy_authority(self):
        release = (ROOT / '.github/workflows/live-promote.yml').read_text()
        validation = release.split('  validation:\n', 1)[1].split('  site-build:\n', 1)[0]
        self.assertIn('uses: ./.github/workflows/live-validation.yml', validation)
        for forbidden in ('secrets:', 'environment:', 'concurrency:', 'run:'):
            self.assertNotIn(forbidden, validation)
        docs = (ROOT / '.github/workflows/docs-deploy.yml').read_text()
        for forbidden in ('scripts/test-', 'cargo test', 'cargo clippy', 'cargo nextest',
                          'smoke-pty-runtime.sh', 'compat-matrix.sh', 'smoke-live-local.py'):
            self.assertNotIn(forbidden, docs)

    def test_shared_release_uses_one_gate_but_standalone_site_requires_review(self):
        release = (ROOT / '.github/workflows/live-promote.yml').read_text()
        docs = (ROOT / '.github/workflows/docs-deploy.yml').read_text()
        approval = docs.split('  approve:\n', 1)[1].split('  deploy:\n', 1)[0]
        deploy = docs.split('  deploy:\n', 1)[1]
        self.assertIn('!inputs.publish_candidate', approval)
        self.assertIn("inputs.publish_candidate && needs.approve.result == 'skipped'", deploy)
        self.assertIn("!inputs.publish_candidate && needs.approve.result == 'success'", deploy)
        self.assertIn("github.workflow_ref == 'BSteffaniak/bmux/.github/workflows/live-promote.yml@refs/heads/master'", deploy)
        self.assertIn("needs.production.result == 'success'", release.split('  site:\n', 1)[1])
        self.assertIn('publish_candidate: true', release.split('  site:\n', 1)[1])
        self.assertEqual(release.count('environment: production-approval'), 1)
        self.assertIn("needs.approve.result == 'success'", release.split('  production:\n', 1)[1])

    def test_site_and_backend_have_separate_publishers(self):
        workflow = (ROOT / '.github/workflows/docs-deploy.yml').read_text()
        for forbidden in ('live-deploy.sh', 'R2_STATE_', 'TOFU_', 'setup-opentofu'):
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
        self.assertNotIn('    paths:', release.split('  push:\n', 1)[1].split('  workflow_dispatch:', 1)[0])

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
            self.assertTrue((output / 'site/_headers').is_file())
            # The mocked generator emits no hashed assets: no immutable rules.
            headers = (output / 'site/_headers').read_text()
            self.assertNotIn('immutable', headers.split('\n', 1)[1])
            self.assertIn('/live\n  Cache-Control: no-store', headers)
            self.assertIn('<title>bmux — Live broadcasts and documentation</title>', html)
            self.assertNotIn('MoosicBox', html)
            self.assertFalse((output / 'relay').exists())
            self.assertFalse((output / 'native').exists())
            for line in (output / 'SHA256SUMS').read_text().splitlines():
                digest, name = line.split('  ', 1)
                self.assertEqual(hashlib.sha256((output / name).read_bytes()).hexdigest(), digest)

    def test_obsolete_worker_dev_entrypoint_is_removed(self):
        self.assertFalse((ROOT / 'scripts/live-dev.mjs').exists())
        release = (ROOT / '.github/workflows/live-promote.yml').read_text()
        self.assertNotIn("'infra/live-relay/**'", release)
        product = (ROOT / 'docs/live-broadcasting.md').read_text()
        self.assertNotIn('node scripts/live-dev.mjs', product)
        self.assertIn('wss://live.bmux.dev/v1/viewer/ws', product)
        self.assertIn('/data/relay/authority.json', product)

    def test_acceptance_resolves_pinned_tools_and_passes_relay(self):
        workflow = (ROOT / '.github/workflows/live-validation.yml').read_text()
        for required in ('cargo build --locked -p bmux-live-relay',
                         '--example snapshot --example wire_frame',
                         'cargo metadata --locked --no-deps --format-version 1',
                         'target_directory', '"$target_dir/debug/examples/snapshot" --binary',
                         'scripts/smoke-live-local.py --relay "$target_dir/debug/bmux-live-relay"'):
            self.assertIn(required, workflow)
        self.assertNotIn('CARGO_TARGET_DIR=', workflow)
        self.assertNotIn('target/debug/', workflow)
        # Parse the actual outer bash -c quoting without executing any build.
        run = workflow.split('      - name: Native runtime and live renderer acceptance (no deployment)\n        run: |\n', 1)[1]
        command = '\n'.join(line[10:] for line in run.splitlines())
        result = subprocess.run(['bash', '-n'], input=command, capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_live_tests_and_acceptance_remain_in_independent_ci(self):
        workflow = (ROOT / '.github/workflows/live-validation.yml').read_text()
        for name in ('test-live-bootstrap.py', 'test-live-github-environments.py', 'test-setup-live.py',
                     'test-live-state-recovery.py', 'test-live-state-provision.py', 'test-live-imports.py',
                     'test-live-plan-policy.py', 'test-live-deploy.py', 'test-live-install.py', 'test-live-state.py',
                     'test-archive-live-state.py', 'test-live-smoke.py', 'test-live-deployment-boundary.py',
                     'test-live-fly.py', 'test-live-build-artifact.py', 'smoke-pty-runtime.sh', 'compat-matrix.sh', 'smoke-live-local.py',
                     'fmt -check', 'init -backend=false', 'infra/live-terraform validate'):
            self.assertIn(name, workflow)
        self.assertNotIn('secrets.', workflow)
        self.assertNotIn('needs:', workflow)  # Validation cannot depend on a deployment/approval.


if __name__ == '__main__':
    unittest.main()
