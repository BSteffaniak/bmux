#!/usr/bin/env python3
"""Offline end-to-end interactive setup, no GitHub/Cloudflare calls."""
import argparse
import contextlib
import importlib.util
import io
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch


def load(name, filename):
    spec = importlib.util.spec_from_file_location(name, Path(__file__).with_name(filename))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


s = load('setup', 'setup-live.py')
fakes = load('github_fakes', 'test-live-github-environments.py')
state = load('state_fakes', 'test-live-state-provision.py')
ACCOUNT = 'a' * 32


class SetupTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.args = argparse.Namespace(repo='owner/repo', approval_reviewer='owner', bucket='bmux-opentofu-state',
            state_dir=self.root, publisher_key_file=self.root / 'publisher-key')
        self.gh = fakes.GitHub()
        self.cf = state.CF()
        self.creations = []
        self.backend_calls = []
        self.fail_secret = None
        self.setup = s.Setup(self.args, self.run_command, {'CLOUDFLARE_PROVISION_TOKEN': 'offline-provision'})
        def r2(*args):
            self.creations.append('r2')
            return dict(R2_STATE_ACCESS_KEY_ID='a' * 32, R2_STATE_SECRET_ACCESS_KEY='b' * 64)
        def token(method, path, body=None):
            if method == 'POST' and path == '/user/tokens':
                self.creations.append('deployment')
                return dict(result=dict(value='offline-deployment-token'))
            return self.cf.original_request(method, path, body)
        self.cf.original_request = self.cf.request
        self.cf.request = token
        self.cf.items = lambda path: []
        patches = [patch.object(s.b, 'Cloudflare', return_value=self.cf),
                   patch.object(s.b, 'discover', return_value=(ACCOUNT, 'c' * 32)),
                   patch.object(s.b, 'r2_credentials', side_effect=r2),
                   patch.object(s.b, 'token_policy', return_value={}),
                   patch.object(s.b, 'pages_project', return_value='bmux'),
                   patch.object(s, 'secret', return_value='offline-high-entropy-passphrase-' * 2),
                   patch.object(s, 'confirm'), contextlib.redirect_stdout(io.StringIO())]
        for item in patches:
            item.__enter__()
            self.addCleanup(item.__exit__, None, None, None)

    def run_command(self, cmd, body=None):
        if len(cmd) > 1 and cmd[1].endswith('provision-live-state.py'):
            self.backend_calls.append(cmd)
            choices = dict(repo='owner/repo', bucket=self.args.bucket, region='auto', account_id=ACCOUNT)
            with s.local.lock(self.root):
                obj = s.p.Provision(choices, self.root, self.cf)
                if '--plan' in cmd:
                    result = obj.plan()
                elif '--apply' in cmd:
                    result = obj.apply(cmd[cmd.index('--apply') + 1], True)
                elif '--review' in cmd:
                    obj.exact(cmd[cmd.index('--review') + 1])
                    result = obj.result()
                else:
                    self.fail('unexpected backend command')
                if '--completion-file' in cmd:
                    obj.completion(result['operation'], Path(cmd[cmd.index('--completion-file') + 1]))
                return json.dumps(result)
        if cmd[:3] == ['gh', 'secret', 'set'] and cmd[3] == self.fail_secret:
            self.fail_secret = None
            raise s.Error('secret upload ambiguous')
        return self.gh.run(cmd, body)

    def test_fresh_setup_and_repeat_is_verified_noop_without_cloudflare_token(self):
        self.setup.execute()
        self.assertEqual(set(self.gh.secrets), set(s.b.NORMAL_SECRET_NAMES + s.b.STATE_SECRET_NAMES))
        self.assertEqual(self.creations, ['r2', 'deployment'])
        inputs = s.local.private_read(self.root / 'configure-inputs.json')
        self.assertEqual(inputs['bootstrap_completion'], (self.root / 'ready.json').read_text())
        self.assertEqual(inputs['bootstrap_completion_sha256'], s.local.digest(self.root / 'ready.json'))
        self.assertEqual(s.local.private_read(self.root / 'setup.json')['phase'], 'complete')
        self.gh.calls.clear()
        self.setup.env.clear()
        with patch.object(s, 'secret', side_effect=AssertionError('unexpected prompt')), \
                patch.object(s.b, 'Cloudflare', side_effect=AssertionError('unexpected cloudflare')):
            self.setup.execute()
        self.assertEqual(self.gh.writes(), [])
        self.assertEqual(self.creations, ['r2', 'deployment'])
        self.assertEqual((self.root / 'credentials.json').stat().st_mode & 0o777, 0o600)
        self.assertNotIn('offline-deployment-token', (self.root / 'setup.json').read_text())
        self.assertNotIn('offline-deployment-token', (self.root / 'configure-inputs.json').read_text())

    def test_partial_secret_upload_resumes_exact_original_pair_and_tokens(self):
        self.fail_secret = 'R2_STATE_SECRET_ACCESS_KEY'
        with self.assertRaises(s.Error):
            self.setup.execute()
        self.assertIn('R2_STATE_ACCESS_KEY_ID', self.gh.secrets)
        self.assertNotIn('R2_STATE_SECRET_ACCESS_KEY', self.gh.secrets)
        original = dict(self.gh.secrets)
        self.setup.execute()
        self.assertEqual(self.creations, ['r2', 'deployment'])
        for name, value in original.items():
            self.assertEqual(self.gh.secrets[name], value)

    def test_credential_creation_ambiguity_never_retries(self):
        def ambiguous():
            raise s.Error('provider timeout')
        with self.assertRaises(s.Error):
            self.setup.retained('deployment', ambiguous)
        with self.assertRaises(s.Error):
            self.setup.retained('deployment', lambda: self.fail('must not retry token creation'))

    def test_prompt_cancel_does_not_poison_passphrase_or_mutate_provider(self):
        with patch.object(s, 'confirm', side_effect=s.Error('declined')), self.assertRaises(s.Error):
            self.setup.execute()
        self.assertTrue(all(method == 'GET' for method, _, _ in self.cf.calls))
        self.assertEqual(self.gh.writes(), [])
        with self.assertRaises(s.Error):
            self.setup.retained('passphrase', lambda: (_ for _ in ()).throw(s.Error('cancel')))
        self.assertEqual(self.setup.retained('passphrase', lambda: 'original-passphrase'), 'original-passphrase')

    def test_lost_authority_corrupt_receipt_and_wrong_selection_fail_closed(self):
        self.setup.execute()
        (self.root / 'ready.json').write_text('{}')
        with self.assertRaises(s.Error):
            self.setup.execute()
        self.assertEqual(self.creations, ['r2', 'deployment'])
        self.args.bucket = 'other-bucket'
        other = s.Setup(self.args, self.run_command, {})
        with self.assertRaises(s.Error):
            other.execute()

    def test_ambiguous_backend_requires_explicit_reconcile(self):
        self.setup.execute()
        ledger = s.local.private_read(self.root / 'operation.json')
        ledger['phase'] = 'applying'
        s.local.save(self.root / 'operation.json', ledger)
        self.gh.secrets.pop('LIVE_HOSTNAME')
        before = len(self.backend_calls)
        with self.assertRaises(s.Error):
            self.setup.execute()
        self.assertEqual(len(self.backend_calls), before)

    def test_verify_detects_drift_without_mutation(self):
        self.setup.execute()
        self.gh.envs['Production']['config']['can_admins_bypass'] = True
        self.gh.calls.clear()
        with self.assertRaises(s.g.Error):
            self.setup.execute()
        self.assertEqual(self.gh.writes(), [])

    def test_bound_selection_and_lost_operation_do_not_create_replacement(self):
        self.setup.execute()
        (self.root / 'operation.json').unlink()
        with self.assertRaises(s.local.InstallError):
            self.setup.execute()
        self.gh.secrets.pop('LIVE_HOSTNAME')
        before = len(self.backend_calls)
        with self.assertRaises(s.Error):
            self.setup.execute()
        self.assertEqual(len(self.backend_calls), before)

    def test_partial_preexisting_state_credentials_block_empty_backend(self):
        self.gh.envs['Production'] = dict(id=3, config=dict(can_admins_bypass=False), branches=[])
        self.gh.secrets['LIVE_STATE_BUCKET'] = 'existing'
        with self.assertRaises(s.Error):
            self.setup.execute()
        self.assertFalse(any('--plan' in cmd for cmd in self.backend_calls))

    def test_lost_publisher_key_is_not_regenerated(self):
        self.setup.execute()
        self.args.publisher_key_file.unlink()
        with self.assertRaises(s.b.BootstrapError):
            self.setup.execute()
        self.assertFalse(self.args.publisher_key_file.exists())

    def test_process_environment_reaches_companion_without_global_secret_exports(self):
        import sys
        result = s.b.command([sys.executable, '-c', 'import os; print(os.environ["OFFLINE_SETUP_TEST"])'],
                             environ={'OFFLINE_SETUP_TEST': 'passed'})
        self.assertEqual(result.strip(), 'passed')

    def test_audit_recovery_then_setup_finishes_without_second_bucket_create(self):
        from datetime import datetime, timezone
        import os
        self.fail_secret = 'LIVE_HOSTNAME'
        with self.assertRaises(s.Error):
            self.setup.execute()
        ledger = s.local.private_read(self.root / 'operation.json')
        original_binding, operation = ledger['binding'], ledger['operation']
        ledger.update(phase='applying', observed=None)
        s.local.save(self.root / 'operation.json', ledger)
        created = s.r.instant(self.cf.row['creation_date'])
        start = created.timestamp() - 1
        os.utime(self.root / 'operation.json', (start, start))
        (self.root / 'ready.json').unlink()
        audit_id = '4aa591f0-6e2b-4083-9516-dacb6cc4e684'
        audit_time = created.isoformat()
        audit = dict(id=audit_id, when=audit_time, action=dict(type='create', result=True),
                     interface='API', owner=dict(id=ACCOUNT), resource=dict(type='r2.bucket', id=self.args.bucket))
        class Reader:
            def get(inner, path, stage):
                return dict(result=[audit] if '/audit_logs?' in path else self.cf.row)
        self.args.recover_creation_audit, self.args.creation_time = audit_id, audit_time
        before = len([v for v in self.cf.calls if v[0] == 'POST'])
        with patch.object(s.r, 'Reader', return_value=Reader()):
            self.setup.execute()
        recovered = s.local.private_read(self.root / 'operation.json')
        self.assertEqual((recovered['binding'], recovered['operation']), (original_binding, operation))
        self.assertEqual(recovered['phase'], 'complete')
        self.assertEqual(len([v for v in self.cf.calls if v[0] == 'POST']), before)
        self.assertEqual(s.local.private_read(self.root / 'setup.json')['phase'], 'complete')
        self.assertEqual(self.creations, ['r2', 'deployment'])

    def test_setup_does_not_dispatch_workflows(self):
        self.setup.execute()
        self.assertFalse(any('workflow' in cmd for cmd, _ in self.gh.calls))


if __name__ == '__main__':
    unittest.main()
