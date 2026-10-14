#!/usr/bin/env python3
"""Focused offline tests: no provider calls, repository mutation or credential access."""
import argparse
import contextlib
import importlib.util
import io
import json
import os
from pathlib import Path
import tempfile
import unittest

spec = importlib.util.spec_from_file_location('bootstrap', Path(__file__).with_name('bootstrap-live-secrets.py'))
m = importlib.util.module_from_spec(spec)
spec.loader.exec_module(m)


def args(**kwargs):
    values = dict(repo='owner/repo', environment='production', only=['CLOUDFLARE_API_TOKEN'],
                  rotate=False, dry_run=False, publisher_key_file=None)
    values.update(kwargs)
    return argparse.Namespace(**values)


class BootstrapTests(unittest.TestCase):
    def runner(self, existing=(), protected=True):
        calls = []
        def run(argv, body=None):
            calls.append((argv, body))
            if argv[:3] == ['gh', 'secret', 'list']:
                return json.dumps([{'name': name} for name in existing])
            if 'deployment-branch-policies' in argv[-1]:
                return json.dumps({'branch_policies': [{'name': 'master', 'type': 'branch'}]})
            if argv[:2] == ['gh', 'api']:
                return json.dumps({'deployment_branch_policy': {'custom_branch_policies': True},
                                   'can_admins_bypass': False,
                                   'protection_rules': [{'type': 'required_reviewers',
                                     'reviewers': [1] if protected else [], 'prevent_self_review': True}]})
            return ''
        return run, calls

    def test_existing_skipped_without_reading_input(self):
        run, calls = self.runner(['CLOUDFLARE_API_TOKEN'])
        with contextlib.redirect_stdout(io.StringIO()):
            m.provision(args(), run, {})
        self.assertFalse(any(c[0][:3] == ['gh', 'secret', 'set'] for c in calls))

    def test_target_and_stdin(self):
        run, calls = self.runner()
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            m.provision(args(), run, {'CLOUDFLARE_API_TOKEN': 'super-private-value'})
        write = calls[-1]
        self.assertEqual(write, (['gh', 'secret', 'set', 'CLOUDFLARE_API_TOKEN', '--repo',
                                 'owner/repo', '--env', 'production'], 'super-private-value'))
        self.assertNotIn('super-private-value', output.getvalue())
        self.assertFalse(any('PUT' in c[0] for c in calls))

    def test_never_weakens_protections(self):
        run, calls = self.runner(protected=False)
        with self.assertRaises(m.BootstrapError):
            m.provision(args(), run, {'CLOUDFLARE_API_TOKEN': 'private'})
        self.assertFalse(any(c[0][:3] == ['gh', 'secret', 'set'] for c in calls))

    def test_rotation_selected_only(self):
        run, calls = self.runner(['CLOUDFLARE_API_TOKEN'])
        with contextlib.redirect_stdout(io.StringIO()):
            m.provision(args(rotate=True), run, {'CLOUDFLARE_API_TOKEN': 'replacement'})
        self.assertEqual(len([c for c in calls if c[1]]), 1)
        self.assertFalse(any('DELETE' in c[0] for c in calls))

    def test_dry_run_no_api(self):
        def fail(*_):
            self.fail('dry run performed API request')
        with contextlib.redirect_stdout(io.StringIO()):
            m.provision(args(dry_run=True), fail, {})

    def test_key_private_digest_only(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'key'
            path.write_text('a' * 64)
            path.chmod(0o600)
            self.assertEqual(len(m.read_private_key(path)), 64)
            path.chmod(0o644)
            with self.assertRaises(m.BootstrapError):
                m.read_private_key(path)
            link = Path(directory) / 'link'
            link.symlink_to(path)
            with self.assertRaises(m.BootstrapError):
                m.read_private_key(link)

    def test_scopes_not_interchangeable(self):
        groups = [dict(name=name, id=name, scopes=[scope]) for name, scope in [
            ('Workers Scripts Write', 'com.cloudflare.api.account'),
            ('Pages Write', 'com.cloudflare.api.account'),
            ('Workers Routes Write', 'com.cloudflare.api.account.zone'),
            ('DNS Write', 'com.cloudflare.api.account.zone')]]
        policy = m.token_policy('account', 'zone', groups)['policies']
        self.assertEqual(list(policy[0]['resources']), ['com.cloudflare.api.account.account'])
        self.assertEqual(list(policy[1]['resources']), ['com.cloudflare.api.account.zone.zone'])
        groups[-1]['scopes'] = ['com.cloudflare.api.account']
        with self.assertRaises(m.BootstrapError):
            m.token_policy('account', 'zone', groups)

    def test_validate_all_before_write(self):
        run, calls = self.runner()
        with self.assertRaises(m.BootstrapError):
            m.provision(args(only=['CLOUDFLARE_API_TOKEN', 'LIVE_STATE_BUCKET']), run,
                        {'CLOUDFLARE_API_TOKEN': 'secret'})
        self.assertFalse(any(c[0][:3] == ['gh', 'secret', 'set'] for c in calls))

    def test_redacted_process_failure(self):
        from unittest.mock import patch
        import subprocess
        with patch.object(m.subprocess, 'run', return_value=subprocess.CompletedProcess([], 1, 'secret', 'token')):
            with self.assertRaises(m.BootstrapError) as error:
                m.command(['gh', 'secret', 'set', 'name'], 'credential')
        self.assertNotIn('token', str(error.exception))
        self.assertNotIn('credential', str(error.exception))


if __name__ == '__main__':
    unittest.main()
