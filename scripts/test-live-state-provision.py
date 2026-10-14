#!/usr/bin/env python3
"""Offline provisioning contract tests; all AWS/OpenTofu commands are mocked."""
import argparse
import contextlib
import importlib.util
import io
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import Mock, patch

spec = importlib.util.spec_from_file_location('provision', Path(__file__).with_name('provision-live-state.py'))
m = importlib.util.module_from_spec(spec)
spec.loader.exec_module(m)
C = dict(repo='owner/repo', bucket='bmux-private-production', region='us-east-1',
         role='bmux-live-state-production', account_id='123456789012')
SECRET = 'REUSABLE_SECRET_NOT_FOR_OUTPUT'


class Tools:
    def __init__(self):
        self.aws = '/mock/aws'
        self.calls = []
        self.fail = False
        self.diff = 0
        self.provider = False
        self.existing = False

    def check(self, account):
        self.calls.append(['check', account])

    def aws_json(self, *args):
        self.calls.append(list(args))
        account = C['account_id']
        if args[1] == 'list-open-id-connect-providers':
            return {'OpenIDConnectProviderList': [{'Arn': f'arn:aws:iam::{account}:oidc-provider/token.actions.githubusercontent.com'}] if self.provider else []}
        if args[1] == 'get-open-id-connect-provider':
            return {'Url': 'token.actions.githubusercontent.com', 'ClientIDList': ['sts.amazonaws.com']}
        if args[1] == 'list-buckets':
            return {'Buckets': [{'Name': C['bucket']}] if self.existing else []}
        if args[1] == 'get-bucket-location':
            return {'LocationConstraint': None}
        if args[1] == 'list-roles':
            return {'Roles': [{'RoleName': C['role'], 'Arn': m.public(C)['role_arn']}] if self.existing else []}
        if args[1] == 'list-attached-role-policies':
            return {'AttachedPolicies': []}
        if args[1] == 'list-role-policies':
            return {'PolicyNames': []}
        if args[1] == 'list-policies':
            return {'Policies': []}
        raise AssertionError(args)

    def run(self, args, **kwargs):
        self.calls.append(args)
        if 'get-public-access-block' in args:
            return subprocess.CompletedProcess(args, 254, b'', b'An error occurred (NoSuchPublicAccessBlockConfiguration)')
        return subprocess.CompletedProcess(args, 254, b'', b'An error occurred (NoSuchBucketPolicy)')

    def tofu_run(self, workspace, *args, codes=(0,)):
        self.calls.append(list(args))
        if args[0] == 'plan':
            for arg in args:
                if arg.startswith('-out='):
                    Path(arg[5:]).write_bytes(b'exact offline plan')
            return subprocess.CompletedProcess(args, self.diff if '-detailed-exitcode' in args else 0, b'', b'')
        if args[0] == 'show':
            value = {'resource_changes': [{'address': 'aws_iam_role.github_production[0]',
                     'change': {'actions': ['create'], 'after': {'secret': SECRET}}}]}
            return subprocess.CompletedProcess(args, 0, json.dumps(value).encode(), b'')
        if args[0] == 'apply' and self.fail:
            raise m.Error('Interrupted mock provider apply; diagnostics suppressed')
        return subprocess.CompletedProcess(args, 0, b'', b'')


class ProvisionTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name) / 'private'
        m.local.private_dir(self.root)
        self.tools = Tools()
        self.addCleanup(self.temp.cleanup)

    def provision(self):
        return m.Provision(C, self.root, self.tools)

    def test_provider_free_default_no_state_or_credentials_output(self):
        output = io.StringIO()
        with patch.object(m, 'Tools', side_effect=AssertionError('default must not invoke provider')), \
                patch.dict(os.environ, {'AWS_SECRET_ACCESS_KEY': SECRET}, clear=True), contextlib.redirect_stdout(output):
            m.main(['--repo', C['repo'], '--bucket', C['bucket'], '--region', C['region'], '--account-id', C['account_id'],
                    '--state-dir', str(self.root / 'absent')])
        self.assertFalse((self.root / 'absent').exists())
        value = json.loads(output.getvalue())
        self.assertEqual(value['phase'], 'dryrun')
        self.assertTrue(value['backend']['use_lockfile'])
        self.assertNotIn(SECRET, output.getvalue())

    def test_plan_review_exact_apply_cost_and_complete_retry(self):
        p = self.provision()
        result = p.plan(False)
        self.assertEqual(result['phase'], 'planned')
        self.assertFalse(any(c[0] == 'apply' for c in self.tools.calls))
        self.assertNotIn(SECRET, json.dumps(p.review()))
        with self.assertRaises(m.Error):
            p.apply('0' * 32, True)
        with self.assertRaises(m.Error):
            p.apply(result['operation'], False)
        p.apply(result['operation'], True)
        count = len(self.tools.calls)
        self.provision().apply(result['operation'], True)
        self.assertEqual(count, len(self.tools.calls))

    def test_partial_apply_no_blind_replay_readonly_reconcile(self):
        p = self.provision()
        op = p.plan(False)['operation']
        self.tools.fail = True
        with self.assertRaises(m.Error):
            p.apply(op, True)
        restored = self.provision()
        self.assertEqual(restored.ledger['phase'], 'applying')
        with self.assertRaises(m.Error):
            restored.apply(op, True)
        self.tools.diff = 2
        with self.assertRaises(m.Error):
            restored.reconcile(op)
        self.assertEqual(restored.ledger['phase'], 'applying')
        self.tools.diff = 0
        restored.reconcile(op)
        self.assertEqual(restored.ledger['phase'], 'complete')
        self.assertEqual(sum(c[0] == 'apply' for c in self.tools.calls), 1)

    def test_existing_provider_reused_and_resources_require_import_approval(self):
        self.tools.provider = self.tools.existing = True
        p = self.provision()
        with self.assertRaisesRegex(m.Error, '--approve-import'):
            p.plan(False)
        result = p.plan(True)
        self.assertEqual(result['phase'], 'planned')
        self.assertIn('oidc-provider/token.actions.githubusercontent.com', p.ledger['provider_arn'])
        self.assertIn('aws_iam_role.github_production[0]', p.ledger['imports'])
        variables = m.local.private_read(p.workspace / 'choices.auto.tfvars.json')
        self.assertEqual(variables['environments'], ['production'])
        self.assertEqual(variables['existing_buckets'], {'production': C['bucket']})
        self.assertNotEqual(variables['github_oidc_provider_arn'], '')

    def test_existing_provider_incompatible_never_recreated(self):
        self.tools.provider = True
        original = self.tools.aws_json
        def answer(*args):
            if args[1] == 'get-open-id-connect-provider':
                return {'Url': 'token.actions.githubusercontent.com', 'ClientIDList': ['other']}
            return original(*args)
        with patch.object(self.tools, 'aws_json', side_effect=answer):
            with self.assertRaises(m.Error):
                self.provision().plan(True)
        self.assertFalse(any(c[0] == 'plan' for c in self.tools.calls))

    def test_existing_role_broad_policy_rejected(self):
        self.tools.existing = True
        original = self.tools.aws_json
        def answer(*args):
            if args[1] == 'list-attached-role-policies':
                return {'AttachedPolicies': [{'PolicyArn': 'arn:aws:iam::aws:policy/AdministratorAccess'}]}
            return original(*args)
        with patch.object(self.tools, 'aws_json', side_effect=answer):
            with self.assertRaisesRegex(m.Error, 'unrelated permissions'):
                self.provision().plan(True)

    def test_bucket_policy_permission_error_not_absence(self):
        self.tools.existing = True
        with patch.object(self.tools, 'run', return_value=subprocess.CompletedProcess([], 254, b'', SECRET.encode())):
            with self.assertRaises(m.Error) as error:
                self.provision().plan(True)
        self.assertNotIn(SECRET, str(error.exception))

    def test_plan_tamper_and_choice_conflict(self):
        p = self.provision()
        op = p.plan(False)['operation']
        (p.workspace / 'reviewed.plan').write_text('tampered')
        with self.assertRaises(m.Error):
            p.apply(op, True)
        with self.assertRaises(m.Error):
            m.Provision(C | {'repo': 'other/repo'}, self.root, self.tools)
        self.assertFalse(any(c[0] == 'apply' for c in self.tools.calls))

    def test_corrupt_ledger_unknown_schema_and_secret_field(self):
        p = self.provision()
        p.plan(False)
        data = dict(p.ledger)
        data['secret'] = SECRET
        m.local.save(p.path, data)
        with self.assertRaises(m.Error):
            self.provision()
        data.pop('secret')
        data['schema'] = 2
        m.local.save(p.path, data)
        with self.assertRaises(m.Error):
            self.provision()

    def test_ledger_private_secret_free(self):
        p = self.provision()
        with patch.dict(os.environ, {'AWS_SECRET_ACCESS_KEY': SECRET}):
            p.plan(False)
        self.assertNotIn(SECRET, p.path.read_text())
        self.assertEqual(p.path.stat().st_mode & 0o777, 0o600)

    def test_account_root_guards(self):
        tools = m.Tools(C['region'])
        tools.aws, tools.tofu = '/mock/aws', '/mock/tofu'
        version = subprocess.CompletedProcess([], 0, b'{"terraform_version":"1.10.9"}', b'')
        for response in ({'Account': C['account_id'], 'Arn': f'arn:aws:iam::{C["account_id"]}:root'},
                         {'Account': '999999999999', 'Arn': 'arn:aws:iam::999999999999:user/test'}):
            with patch.object(tools, 'run', return_value=version), patch.object(tools, 'aws_json', return_value=response):
                with self.assertRaises(m.Error):
                    tools.check(C['account_id'])

    def test_process_redaction_timeout(self):
        tools = m.Tools(C['region'])
        with self.assertRaises(m.Error) as error:
            tools.run([os.sys.executable, '-c', 'import sys; print(sys.argv[1]); sys.exit(1)', SECRET])
        self.assertNotIn(SECRET, str(error.exception))
        with self.assertRaises(m.Error) as error:
            tools.run([os.sys.executable, '-c', 'import time; time.sleep(10)'], timeout=0.02)
        self.assertNotIn(SECRET, str(error.exception))

    def test_aggregate_deadline_times_out_and_reaps_process(self):
        tools = m.Tools(C['region'])
        tools.deadline = m.time.monotonic() + 0.03
        spawned = []
        original = m.subprocess.Popen
        def capture(*args, **kwargs):
            process = original(*args, **kwargs)
            spawned.append(process)
            return process
        with patch.object(m.subprocess, 'Popen', side_effect=capture):
            with self.assertRaises(m.Error):
                tools.run([os.sys.executable, '-c', 'import time; time.sleep(10)'], timeout=900)
        self.assertEqual(len(spawned), 1)
        self.assertIsNotNone(spawned[0].poll())
        self.assertLess(m.time.monotonic() - tools.deadline, 2)

    def test_aggregate_budget_not_reset_between_calls_no_spawn_after_expiry(self):
        tools = m.Tools(C['region'])
        deadline = m.time.monotonic() + 2
        tools.deadline = deadline
        tools.run([os.sys.executable, '-c', 'pass'])
        self.assertEqual(tools.deadline, deadline)
        tools.deadline = m.time.monotonic() - 1
        with patch.object(m.subprocess, 'Popen', side_effect=AssertionError('expired budget must not spawn')):
            with self.assertRaisesRegex(m.Error, 'Aggregate provisioning deadline'):
                tools.run([os.sys.executable, '-c', 'pass'])

    def test_output_limit_and_truncated_inventory_fail_closed(self):
        tools = m.Tools(C['region'])
        with self.assertRaises(m.Error):
            tools.run([os.sys.executable, '-c', 'import sys; sys.stdout.buffer.write(b"x" * (33*1024*1024))'])
        tools.aws = '/mock/aws'
        result = subprocess.CompletedProcess([], 0, b'{"Roles":[],"IsTruncated":true}', b'')
        with patch.object(tools, 'run', return_value=result):
            with self.assertRaises(m.Error):
                tools.aws_json('iam', 'list-roles')

    def test_existing_env_authority_no_duplicate(self):
        args = argparse.Namespace(repo=C['repo'], bucket=None, region=None, role=C['role'], account_id=C['account_id'])
        with patch.dict(os.environ, {'LIVE_STATE_BUCKET': C['bucket'], 'LIVE_STATE_REGION': C['region']}, clear=True):
            self.assertEqual(m.choices(args), C)
            args.bucket = 'different-bucket'
            with self.assertRaises(m.Error):
                m.choices(args)

    def test_exact_claims_and_no_credentials_or_wildcard(self):
        value = m.public(C)
        self.assertEqual(value['github_oidc']['subject'], 'repo:owner/repo:environment:Production:ref:refs/heads/master')
        self.assertEqual(value['github_oidc']['include_claim_keys'], ['repo', 'context', 'ref'])
        text = (m.MODULE / 'main.tf').read_text()
        self.assertIn('"token.actions.githubusercontent.com:aud" = "sts.amazonaws.com"', text)
        self.assertIn('"repo:${var.github_repository}:environment:Production:ref:refs/heads/master"', text)
        self.assertNotIn('StringLike = {\n      "token.actions', text)
        self.assertNotIn('aws_iam_access_key', text)
        self.assertIn('terraform.tfstate.tflock', text)
        self.assertIn('versioning_configuration { status = "Enabled" }', text)
        self.assertIn('"staging", "production"', text)

    def test_destructive_or_unexpected_plan_blocked(self):
        self.assertRaises(m.Error, m.local.review_summary, {'resource_changes': [{'address': 'aws_iam_role.github_production[0]', 'change': {'actions': ['delete', 'create']}}]})
        for value in [{'aws_iam_role.github_production[0]': 'other-role'}, {'aws_iam_access_key.secret': SECRET}]:
            with self.assertRaises(m.Error):
                m.validate_imports(value, C)

    def test_workspace_symlink_and_module_identity_fail_closed(self):
        p = self.provision()
        operation = p.plan(False)['operation']
        outside = self.root / 'outside'
        outside.write_text('keep')
        target = p.workspace / 'main.tf'
        target.unlink()
        target.symlink_to(outside)
        with self.assertRaises(m.Error):
            p.apply(operation, True)
        self.assertEqual(outside.read_text(), 'keep')
        with patch.object(m.local, 'digest', return_value='0' * 64):
            with self.assertRaises(m.Error):
                p.apply(operation, True)
        self.assertFalse(any(c[0] == 'apply' for c in self.tools.calls))

    def test_dryrun_executable_no_cloud(self):
        result = subprocess.run([os.sys.executable, str(Path(__file__).with_name('provision-live-state.py')),
                                 '--repo', C['repo'], '--bucket', C['bucket'], '--region', C['region'], '--account-id', C['account_id']],
                                capture_output=True, check=False, env={k: v for k, v in os.environ.items() if not k.startswith('LIVE_STATE_')})
        self.assertEqual(result.returncode, 0)
        self.assertEqual(json.loads(result.stdout)['phase'], 'dryrun')
        self.assertNotIn(b'Traceback', result.stderr)


if __name__ == '__main__':
    unittest.main()
