#!/usr/bin/env python3
"""Real offline bootstrap CLI and credential safety tests."""
import argparse
import contextlib
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

HERE = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location('bootstrap', HERE / 'bootstrap-live-secrets.py')
b = importlib.util.module_from_spec(spec)
spec.loader.exec_module(b)
ACCOUNT = 'a' * 32


class BootstrapTests(unittest.TestCase):
    def test_cli_dryrun_no_executables_or_credentials(self):
        cmd = [sys.executable, str(HERE / 'bootstrap-live-secrets.py'), '--repo', 'owner/repo',
               '--bootstrap-state', '--dry-run']
        result = subprocess.run(cmd, capture_output=True, text=True, env={'PATH': '/nonexistent'}, timeout=10)
        self.assertEqual(result.returncode, 0, result.stderr)
        data = json.loads(result.stdout)
        self.assertFalse(data['provider_requests'])
        for name in ('R2_STATE_ACCESS_KEY_ID', 'R2_STATE_SECRET_ACCESS_KEY', 'TOFU_STATE_ENCRYPTION_PASSPHRASE'):
            self.assertIn(name, data['secrets'])
        self.assertNotIn('ROLE', result.stdout)

    def test_deployment_token_includes_scoped_r2_management(self):
        groups = [dict(name=n, id='b' * 32, scopes=[scope]) for scope, names in (
            ('com.cloudflare.api.account', ('Workers Scripts Write', 'Pages Write', 'Workers R2 Storage Write')),
            ('com.cloudflare.api.account.zone', ('Workers Routes Write', 'DNS Write', 'Zone Read')))
            for n in names]
        policy = b.token_policy(ACCOUNT, 'c' * 32, groups)
        self.assertEqual(len(policy['policies'][0]['permission_groups']), 3)
        self.assertEqual(policy['policies'][0]['resources'], {f'com.cloudflare.api.account.{ACCOUNT}': '*'})
        with self.assertRaises(b.BootstrapError):
            b.token_policy(ACCOUNT, 'c' * 32, [g for g in groups if g['name'] != 'Workers R2 Storage Write'])

    def test_obsolete_flags_reject(self):
        for flag in ('--configure-oidc', '--state-role'):
            result = subprocess.run([sys.executable, str(HERE / 'bootstrap-live-secrets.py'),
                                     '--repo', 'owner/repo', '--dry-run', flag], capture_output=True, timeout=10)
            self.assertNotEqual(result.returncode, 0)

    def test_scoped_token_derivation(self):
        calls = []
        class CF:
            def items(self, path):
                return [dict(name='Workers R2 Storage Bucket Item Write', id='b' * 32,
                             scopes=['com.cloudflare.edge.r2.bucket'])]
            def request(self, method, path, body):
                calls.append((method, path, body))
                return {'result': {'id': 'c' * 32, 'value': 'private-token-value-not-for-output'}}
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            result = b.r2_credentials(CF(), ACCOUNT, 'bmux-opentofu-state')
        self.assertEqual(out.getvalue(), '')
        self.assertEqual(result['R2_STATE_SECRET_ACCESS_KEY'], hashlib.sha256(b'private-token-value-not-for-output').hexdigest())
        resources = calls[0][2]['policies'][0]['resources']
        self.assertEqual(resources, {f'com.cloudflare.edge.r2.bucket.{ACCOUNT}_default_bmux-opentofu-state': '*'})

    def test_bad_permission_scope_reject(self):
        with self.assertRaises(b.BootstrapError):
            b.r2_token_policy(ACCOUNT, 'bucket', [dict(name='Workers R2 Storage Bucket Item Write',
                                                    id='b' * 32, scopes=['com.cloudflare.api.account'])])

    def args(self, **kwargs):
        values = dict(repo='owner/repo', environment='Production', only=['R2_STATE_ACCESS_KEY_ID', 'R2_STATE_SECRET_ACCESS_KEY'],
                      rotate=False, dry_run=False, input_env=True, publisher_key_file=None)
        values.update(kwargs)
        return argparse.Namespace(**values)

    def run_stub(self, existing, calls):
        def run(args, body=None):
            calls.append((args, body))
            if args[:3] == ['gh', 'secret', 'list']:
                return json.dumps([{'name': name} for name in existing])
            return ''
        return run

    def test_env_secrets_stdin_and_skip(self):
        from unittest.mock import patch
        calls = []
        env = dict(R2_STATE_ACCESS_KEY_ID='a' * 32, R2_STATE_SECRET_ACCESS_KEY='b' * 64, CLOUDFLARE_ACCOUNT_ID=ACCOUNT, LIVE_STATE_BUCKET='bmux-opentofu-state')
        with patch.object(b, 'verify_environment'), contextlib.redirect_stdout(io.StringIO()):
            b.provision_env(self.args(), self.run_stub([], calls), env)
        writes = [(args, body) for args, body in calls if args[:3] == ['gh', 'secret', 'set']]
        self.assertEqual(len(writes), 2)
        for args, body in writes:
            self.assertNotIn(body, args)
        calls.clear()
        with patch.object(b, 'verify_environment'), contextlib.redirect_stdout(io.StringIO()):
            b.provision_env(self.args(), self.run_stub(['R2_STATE_ACCESS_KEY_ID', 'R2_STATE_SECRET_ACCESS_KEY'], calls), {})
        self.assertFalse(any(args[:3] == ['gh', 'secret', 'set'] for args, _ in calls))

    def test_legacy_and_passphrase_rotation_reject_without_writes(self):
        from unittest.mock import patch
        for existing, args in ((['LIVE_STATE_ROLE_ARN'], self.args()),
                               ([], self.args(rotate=True, only=['TOFU_STATE_ENCRYPTION_PASSPHRASE']))):
            calls = []
            with patch.object(b, 'verify_environment'), self.assertRaises(b.BootstrapError):
                b.provision_env(args, self.run_stub(existing, calls), {})
            self.assertFalse(any(cmd[:3] == ['gh', 'secret', 'set'] for cmd, _ in calls))

    def test_plan_review_apply_bound_metadata(self):
        args = self.args(state_bucket=None, state_region='auto', state_dir=Path('/private'),
                         state_account_id=ACCOUNT, state_mode='plan', state_operation=None,
                         accept_cost=False, approve_import=False)
        op = 'd' * 32
        calls = []
        backend = dict(bucket='bmux-opentofu-state', region='auto', key='bmux/live/production/terraform.tfstate',
                       endpoint=f'https://{ACCOUNT}.r2.cloudflarestorage.com', use_lockfile=True, encrypt=True)
        def run(cmd):
            calls.append(cmd)
            return json.dumps(dict(schema=2, operation=op, phase='complete' if '--apply' in cmd or '--completion-file' in cmd else 'planned',
                                   backend=backend, resources=[{'address': 'cloudflare_r2_bucket.state',
                                                               'actions': ['create'], 'import': False}]))
        with contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(b.state_bootstrap(args, run, ['LIVE_STATE_BUCKET'], {}), {})
            args.state_mode, args.state_operation, args.accept_cost = 'apply', op, True
            values = b.state_bootstrap(args, run, ['LIVE_STATE_BUCKET'], {})
        self.assertEqual(values['LIVE_STATE_BUCKET'], 'bmux-opentofu-state')
        self.assertTrue(any('--accept-cost' in cmd for cmd in calls))

    def test_fresh_descriptor_before_secret_writes_without_state_seed(self):
        from unittest.mock import patch
        calls = []
        with tempfile.TemporaryDirectory() as directory:
            args = self.args(input_env=False, zone='bmux.dev', bootstrap_state=True,
                             only=list(b.STATE_SECRET_NAMES), state_bucket=None, state_region='auto',
                             state_dir=Path(directory), state_account_id=None, state_mode='apply',
                             state_operation='d' * 32, accept_cost=True, approve_import=False,
                             state_completion_file=None)
            def run(cmd, body=None, env=None):
                calls.append((cmd, body, env))
                if cmd[:3] == ['gh', 'secret', 'list']:
                    return '[]'
                if len(cmd) > 1 and cmd[1].endswith('provision-live-state.py'):
                    backend = dict(bucket='bmux-opentofu-state', region='auto',
                                   key='bmux/live/production/terraform.tfstate',
                                   endpoint=f'https://{ACCOUNT}.r2.cloudflarestorage.com', use_lockfile=True, encrypt=True)
                    return json.dumps(dict(schema=2, operation='d' * 32,
                                           phase='complete' if '--apply' in cmd or '--completion-file' in cmd else 'planned',
                                           backend=backend, resources=[{'address': 'cloudflare_r2_bucket.state',
                                                                       'actions': ['create'], 'import': False}]))
                return ''
            creds = dict(R2_STATE_ACCESS_KEY_ID='a' * 32, R2_STATE_SECRET_ACCESS_KEY='b' * 64)
            with patch.object(b, 'verify_environment'), patch.object(b, 'discover', return_value=(ACCOUNT, 'c' * 32)), \
                    patch.object(b, 'r2_credentials', return_value=creds), contextlib.redirect_stdout(io.StringIO()):
                b.provision(args, run, {'CLOUDFLARE_PROVISION_TOKEN': 'private-admin-token',
                                        'TOFU_STATE_ENCRYPTION_PASSPHRASE': 'secure-original-passphrase-' * 3}, cloudflare=object())
            completion_index = next(i for i, (cmd, _, _) in enumerate(calls) if '--completion-file' in cmd)
            write_indices = [i for i, (cmd, _, _) in enumerate(calls) if cmd[:3] == ['gh', 'secret', 'set']]
            self.assertTrue(all(completion_index < i for i in write_indices))
            self.assertFalse(any('seed' in cmd or any('archive-live-state.py' in arg for arg in cmd) for cmd, _, _ in calls))
            passphrase_writes = [body for cmd, body, _ in calls if cmd[:4] == ['gh', 'secret', 'set', 'TOFU_STATE_ENCRYPTION_PASSPHRASE']]
            self.assertEqual(passphrase_writes, ['secure-original-passphrase-' * 3])

    def test_apply_requires_passphrase_before_provider_mutation(self):
        from unittest.mock import patch
        with tempfile.TemporaryDirectory() as directory:
            args = self.args(input_env=False, zone='bmux.dev', bootstrap_state=True,
                             only=list(b.STATE_SECRET_NAMES), state_mode='apply', state_dir=Path(directory),
                             state_operation='d' * 32, state_completion_file=None, approve_import=False)
            calls = []
            with patch.object(b, 'verify_environment'), self.assertRaises(b.BootstrapError):
                b.provision(args, self.run_stub([], calls), {})
            self.assertTrue(all(cmd[0] == 'gh' for cmd, _ in calls))
            self.assertFalse(any(cmd[:3] == ['gh', 'secret', 'set'] for cmd, _ in calls))

    def test_secret_safe_subprocess_failure(self):
        with self.assertRaises(b.BootstrapError) as cm:
            b.command([sys.executable, '-c', 'import sys;print("DO_NOT_LEAK");sys.exit(1)'])
        self.assertNotIn('DO_NOT_LEAK', str(cm.exception))


if __name__ == '__main__':
    unittest.main()
