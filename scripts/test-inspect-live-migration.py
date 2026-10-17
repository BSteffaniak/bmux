#!/usr/bin/env python3
"""Read-only inspection tests: fixed output, stable authority, actual decryption."""
import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest
from unittest.mock import Mock, patch

ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location('inspect_migration', ROOT / 'scripts/inspect-live-migration.py')
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)
BODY = json.dumps({'encryption_version': 'v0', 'meta': {'key': {}},
                   'encrypted_data': 'YWFhYWFhYWFhYWFhYWFhYQ=='}).encode()


def store(body=BODY):
    fake = Mock(spec=['unlocked', 'migration_marker', 'get', 'key'])
    fake.key = 'canonical-state'
    fake.get.return_value = (body, 'etag')
    fake.migration_marker.return_value = ({'phase': 'applying', 'private': 'NEVER_PRINT'}, 'marker-etag')
    return fake


class InspectTests(unittest.TestCase):
    def test_workflow_is_manual_read_only_and_build_free(self):
        workflow = (ROOT / '.github/workflows/live-inspect.yml').read_text()
        self.assertIn('workflow_dispatch:', workflow)
        self.assertNotIn('  push:', workflow)
        self.assertIn('environment: Production', workflow)
        self.assertIn('group: bmux-production-infrastructure', workflow)
        self.assertIn('python3 -B scripts/inspect-live-migration.py', workflow)
        for forbidden in ('upload-artifact', 'cargo ', 'tofu apply', 'tofu import',
                          'FLY_API_TOKEN', 'CLOUDFLARE_API_TOKEN', 'live-deploy.sh'):
            self.assertNotIn(forbidden, workflow)

    def test_fixed_output_and_read_only_commands(self):
        fake = store()
        calls = []
        def runner(args, **kwargs):
            calls.append(args)
            self.assertNotIn('R2_STATE_SECRET_ACCESS_KEY', kwargs['env'])
            self.assertNotIn('TF_CLI_ARGS', kwargs['env'])
            return subprocess.CompletedProcess(args, 0,
                'fly_app.live\nprivate_resource.NEVER_PRINT\n'
                if args[1] == 'state' else '', '')
        with patch.dict(os.environ, {'TOFU_STATE_ENCRYPTION_PASSPHRASE': 'private',
                                    'TF_CLI_ARGS': '-unsafe', 'R2_STATE_SECRET_ACCESS_KEY': 'secret'}):
            result = MODULE.inspect(fake, runner)
        self.assertTrue(result['managed_resources']['fly_app.live'])
        self.assertFalse(result['managed_resources']['fly_machine.live'])
        self.assertEqual(result['migration_phase'], 'applying')
        self.assertNotIn('NEVER_PRINT', json.dumps(result))
        self.assertEqual([args[1] for args in calls], ['init', 'state'])

    def test_reject_changed_state_and_unknown_plaintext(self):
        fake = store()
        fake.get.side_effect = [(BODY, 'etag'), (BODY, 'changed')]
        with patch.dict(os.environ, {'TOFU_STATE_ENCRYPTION_PASSPHRASE': 'private'}):
            with self.assertRaisesRegex(ValueError, 'changed'):
                MODULE.inspect(fake, lambda *a, **k: subprocess.CompletedProcess(a, 0, '', ''))
        with self.assertRaises(MODULE.STORE.StateError):
            MODULE.inspect(store(b'{"resources": []}'))

    @unittest.skipUnless(shutil.which('tofu'), 'OpenTofu unavailable')
    def test_real_opentofu_encrypted_local_copy(self):
        # Generate only isolated local test state. No provider or remote backend.
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            (directory / 'encryption.tf').write_bytes((ROOT / 'infra/live-terraform/encryption.tf').read_bytes())
            (directory / 'main.tf').write_text('resource "terraform_data" "probe" { input = "NEVER_PRINT" }\n')
            env = {k: v for k, v in os.environ.items() if not k.startswith(('TF_', 'TOFU_', 'AWS_'))}
            env['TF_VAR_state_encryption_passphrase'] = 'isolated-local-test-passphrase'
            for args in (['init', '-backend=false', '-input=false'], ['apply', '-auto-approve', '-input=false']):
                result = subprocess.run(['tofu', *args], cwd=directory, env=env,
                                        capture_output=True, text=True, timeout=90)
                self.assertEqual(result.returncode, 0, result.stderr)
            body = (directory / 'terraform.tfstate').read_bytes()
            with patch.dict(os.environ, {'TOFU_STATE_ENCRYPTION_PASSPHRASE': env['TF_VAR_state_encryption_passphrase']}):
                def real_runner(args, **kwargs):
                    outcome = subprocess.run(args, **kwargs)
                    self.assertEqual(outcome.returncode, 0, outcome.stderr)
                    return outcome
                result = MODULE.inspect(store(body), real_runner)
            self.assertTrue(result['read_only'])
            self.assertFalse(any(result['managed_resources'].values()))


if __name__ == '__main__':
    unittest.main()
