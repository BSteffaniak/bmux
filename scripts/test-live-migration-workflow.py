#!/usr/bin/env python3
"""Offline guards for the approved migration workflow contract; no cloud access."""
from pathlib import Path
import re
import subprocess
import unittest

ROOT = Path(__file__).resolve().parent.parent


def workflow(name):
    return (ROOT / '.github/workflows' / name).read_text()


def job(source, name):
    match = re.search(r'^  ' + re.escape(name) + r':\n(.*?)(?=^  [\w-]+:\n|\Z)',
                      source.split('jobs:\n', 1)[1], re.M | re.S)
    if not match:
        raise AssertionError('Missing job: ' + name)
    return match.group(1)


class MigrationWorkflowTests(unittest.TestCase):
    def setUp(self):
        self.release = workflow('live-promote.yml')
        self.docs = workflow('docs-deploy.yml')

    def test_explicit_opt_in_and_unchanged_configure_interface(self):
        self.assertEqual(self.release.count('      migrate:\n'), 2)
        for section in ('workflow_call:', 'workflow_dispatch:'):
            source = self.release.split(section, 1)[1]
            self.assertRegex(source, r'(?s)migrate:\n.*?type: boolean\n        default: false')
        configure = workflow('configure-production-infra.yml')
        self.assertIn('configure: true', configure)
        self.assertNotIn('migrate:', configure)
        production = job(self.release, 'production')
        for mode in ('mode=release', 'mode=configure', 'mode=retire-worker', 'mode=migrate'):
            self.assertIn(mode, production)

    def test_exact_revision_validation_is_independent_and_required(self):
        validation = job(self.release, 'validation')
        self.assertIn('uses: ./.github/workflows/live-validation.yml', validation)
        self.assertNotIn('secrets:', validation)
        source = workflow('live-validation.yml')
        self.assertIn('  workflow_call:', source)
        self.assertNotIn('secrets.', source)
        self.assertNotIn('needs:', source)
        self.assertIn('test-live-migration-workflow.py', source)
        self.assertIn('test-live-migration-plan.py', source)
        self.assertIn('test-live-migration-deploy.py', source)
        self.assertIn('test-live-migration-fence.py', source)
        self.assertIn('test-live-guided-setup.py', source)
        self.assertNotIn('inputs.migrate', validation)
        for name in ('preview', 'approve', 'production'):
            self.assertIn('validation', job(self.release, name))
        for name in ('approve', 'production'):
            self.assertIn("needs.validation.result == 'success'", job(self.release, name))

    def test_preview_precedes_approval_with_identical_credentials(self):
        preview = job(self.release, 'preview')
        production = job(self.release, 'production')
        self.assertIn('preview', job(self.release, 'approve'))
        self.assertIn('steps.preview.outputs.migration_digest', preview)
        for source in (preview, production):
            self.assertIn('environment: Production', source)
            self.assertIn('group: bmux-production-infrastructure', source)
            self.assertIn('cancel-in-progress: false', source)
            self.assertIn('sha256sum -c live-candidate.tar.gz.sha256', source)
            self.assertIn('"$GITHUB_SHA"', source)
        credentials = lambda text: set(re.findall(r'\$\{\{ secrets\.([A-Z_0-9]+) \}\}', text))
        self.assertEqual(credentials(preview), credentials(production))
        self.assertIn(' preview-migration', preview)
        self.assertNotIn('sudo apt-get', preview)
        self.assertIn('test -z "$LIVE_BOOTSTRAP_COMPLETION$LIVE_BOOTSTRAP_COMPLETION_SHA256"', preview)
        self.assertIn("test \"$LIVE_REVIEWED_IMPORTS\" = '{}'", preview)
        self.assertIn('LIVE_MIGRATION_DIGEST: ${{ needs.preview.outputs.migration_digest }}', production)
        self.assertIn("(inputs.retire_worker || inputs.migrate) && 'retire-bmux-live-worker'", production)
        self.assertIn('[[ "$LIVE_MIGRATION_DIGEST" =~ ^[a-f0-9]{64}$ ]]', production)

    def test_separate_same_run_site_and_no_nested_lock(self):
        build = job(self.release, 'site-build')
        self.assertIn('scripts/build-site-artifact.sh', build)
        self.assertNotIn('secrets.', build)
        self.assertIn('site-candidate-${{ github.sha }}', build)
        site = job(self.release, 'site')
        self.assertIn('needs: [production, site-build]', site)
        self.assertIn('!inputs.configure && !inputs.retire_worker', site)
        self.assertNotIn('inputs.migrate', site)
        self.assertIn('!inputs.configure && !inputs.retire_worker', build)
        self.assertNotIn('inputs.migrate', build)
        for path in ('packages/docs-site/**', 'packages/web-renderer/**', 'scripts/build-site-artifact.sh',
                     'docs/**', 'README.md'):
            self.assertIn(path, self.release.split('  workflow_dispatch:', 1)[0])
        self.assertIn('uses: ./.github/workflows/docs-deploy.yml', site)
        self.assertIn('reuse_candidate: true', site)
        self.assertNotIn('concurrency:', site)
        self.assertIn('if: ${{ !inputs.reuse_candidate }}', job(self.docs, 'build'))
        for name in ('approve', 'deploy'):
            source = job(self.docs, name)
            self.assertIn("github.event_name != 'push' || inputs.reuse_candidate", source)
            self.assertIn("needs.build.result == 'skipped'", source)
            self.assertIn("github.repository == 'BSteffaniak/bmux'", source)
        self.assertIn('group: bmux-production-pages', job(self.docs, 'deploy'))
        self.assertNotIn('concurrency:', job(self.docs, 'approve'))
        for name in ('validation', 'site-build', 'preview', 'approve', 'production', 'site', 'migration-summary'):
            source = job(self.release, name)
            self.assertIn("github.ref == 'refs/heads/master'", source)
            self.assertIn("github.repository == 'BSteffaniak/bmux'", source)

    def test_public_summary_is_bounded_not_interactive_proof(self):
        summary = job(self.release, 'migration-summary')
        self.assertIn('needs: [production, site]', summary)
        self.assertNotIn('secrets.', summary)
        self.assertIn('timeout-minutes: 5', summary)
        self.assertIn('not proof of a live interactive', summary)
        self.assertIn('do not automatically retry', summary)
        for source in (self.release, self.docs):
            self.assertNotIn('continue-on-error:', source)
            self.assertNotIn('retry', source.replace('do not automatically retry', ''))

    def test_isolated_streaming_acceptance_uses_only_locked_browser(self):
        source = workflow('live-validation.yml')
        acceptance = job(source, 'acceptance')
        for required in ('nix build --no-link --print-out-paths --impure --expr',
                         'builtins.getFlake (toString ./.)',
                         'flake.inputs.nixpkgs.legacyPackages.x86_64-linux.chromium',
                         'test -x "$chromium/bin/chromium"',
                         'bash scripts/dev.sh bash -euo pipefail',
                         'bash scripts/build-site-artifact.sh "$RUNNER_TEMP/acceptance-site"',
                         'cargo build --locked -p bmux-live-relay -p bmux_cli',
                         'scripts/live-browser-acceptance.py', 'scripts/live-native-acceptance.py',
                         '--binary "$target_dir/debug/bmux"', '--sha256 "$binary_sha256"',
                         '--browser "$LIVE_ACCEPTANCE_BROWSER"',
                         '--wire-tool "$target_dir/debug/examples/wire_frame"',
                         '--signal=TERM --kill-after=30s 10m', '--signal=TERM --kill-after=30s 15m',
                         '"$GITHUB_SHA"'):
            self.assertIn(required, acceptance)
        for forbidden in ('apt-get', 'nixpkgs#', 'github:NixOS/nixpkgs', 'secrets.',
                          'environment:', 'live-promote.yml', 'docs-deploy.yml'):
            self.assertNotIn(forbidden, acceptance)
        self.assertNotIn('uses: ./.github/workflows/', source)
        for name in ('approve', 'production'):
            self.assertIn("needs.validation.result == 'success'", job(self.release, name))

    def test_every_workflow_shell_block_parses(self):
        for name in ('live-promote.yml', 'docs-deploy.yml', 'live-validation.yml', 'configure-production-infra.yml'):
            lines = workflow(name).splitlines()
            for index, line in enumerate(lines):
                if line.strip() != 'run: |':
                    continue
                indent = len(line) - len(line.lstrip())
                block = []
                for following in lines[index + 1:]:
                    if following.strip() and len(following) - len(following.lstrip()) <= indent:
                        break
                    block.append(following[indent + 2:])
                result = subprocess.run(['bash', '-n'], input='\n'.join(block), text=True,
                                        capture_output=True, check=False)
                self.assertEqual(result.returncode, 0, (name, index, result.stderr))


if __name__ == '__main__':
    unittest.main()
