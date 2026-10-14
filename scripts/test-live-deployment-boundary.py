#!/usr/bin/env python3
"""Guard build/deploy vs validation separation without requiring a cloud account."""
from pathlib import Path
import re
import unittest

ROOT = Path(__file__).resolve().parent.parent


class DeploymentBoundaryTests(unittest.TestCase):
    def test_candidate_builder_contains_no_test_or_lint_suites(self):
        source = (ROOT / 'scripts/build-live-artifact.sh').read_text()
        for command in (r'cargo\s+(?:clippy|nextest|machete|test|check)\b', r'npm\s+test\b',
                        r'python3\s+scripts/test-', r'(?:bash|python3)\s+scripts/(?:smoke-|compat-matrix)'):
            self.assertIsNone(re.search(command, source), command)
        for required in ('cargo build --locked --release', 'SHA256SUMS', 'git rev-parse HEAD',
                         'plugin rebuild --all-workspace-plugins', 'build-web-renderer.sh'):
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
