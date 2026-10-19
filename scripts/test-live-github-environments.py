#!/usr/bin/env python3
"""Offline GitHub setup policy, drift, recovery and bootstrap integration tests."""
import argparse
import contextlib
import copy
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


g = load('github', 'live-github-environments.py')
b = load('bootstrap', 'bootstrap-live-secrets.py')
OP = 'd' * 32


class GitHub:
    def __init__(self):
        self.envs = {}
        self.calls = []
        self.next_id = 100
        self.fail_after_write = False
        self.fail_before_write = False
        self.custom = []
        self.secrets = {}

    def run(self, args, body=None):
        self.calls.append((args, body))
        if args[:3] == ['gh', 'secret', 'list']:
            return json.dumps([dict(name=v) for v in self.secrets])
        if args[:3] == ['gh', 'secret', 'set']:
            self.secrets[args[3]] = body
            return ''
        assert args[:2] == ['gh', 'api'], args
        method = args[args.index('--method') + 1] if '--method' in args else 'GET'
        path = args[2].split('?')[0]
        if method == 'GET' and path == 'users/owner':
            return json.dumps(dict(login='owner', id=7, type='User'))
        if path == 'repos/owner/repo/environments':
            return json.dumps(dict(total_count=len(self.envs), environments=[dict(name=n) for n in self.envs]))
        name = path.split('/')[4]
        if method == 'GET':
            value = self.envs[name]
            if path.endswith('/deployment-branch-policies'):
                result = dict(total_count=len(value['branches']), branch_policies=value['branches'])
            elif path.endswith('/deployment_protection_rules'):
                result = dict(total_count=len(self.custom), custom_deployment_protection_rules=self.custom)
            else:
                result = dict(name=name, id=value['id'], **copy.deepcopy(value['config']))
                result['protection_rules'] = [dict(type='branch_policy')]
                if value['config'].get('reviewers'):
                    result['protection_rules'].append(dict(type='required_reviewers',
                        prevent_self_review=value['config']['prevent_self_review'],
                        reviewers=[dict(type=r['type'], reviewer=dict(id=r['id'])) for r in value['config']['reviewers']]))
                if value['config'].get('wait_timer'):
                    result['protection_rules'].append(dict(type='wait_timer', wait_timer=value['config']['wait_timer']))
            return json.dumps(result)
        if self.fail_before_write:
            self.fail_before_write = False
            raise b.BootstrapError('ambiguous transport')
        self.next_id += 1
        if method == 'PUT':
            self.envs[name] = dict(id=self.envs[name]['id'] if name in self.envs else self.next_id,
                                   config=json.loads(body), branches=self.envs[name]['branches'] if name in self.envs else [])
        elif method == 'POST':
            self.envs[name]['branches'].append(dict(id=self.next_id, **json.loads(body)))
        elif method == 'DELETE':
            self.envs[name]['branches'] = [v for v in self.envs[name]['branches'] if v['id'] != int(path.split('/')[-1])]
        else:
            raise AssertionError(method)
        if self.fail_after_write:
            self.fail_after_write = False
            raise b.BootstrapError('ambiguous transport')
        return '{}'

    def writes(self):
        return [(cmd, body) for cmd, body in self.calls if '--method' in cmd or cmd[:3] == ['gh', 'secret', 'set']]


def seed(gh):
    """Fixture represents already-applied infra/live-github resources."""
    for i, name in enumerate(('Production', 'production-approval')):
        gh.envs[name] = dict(id=100 + i, config=dict(wait_timer=0,
            reviewers=[dict(type='User', id=7)] if name == 'production-approval' else [],
            prevent_self_review=False, can_admins_bypass=False,
            deployment_branch_policy=dict(protected_branches=False, custom_branch_policies=True)),
            branches=[dict(id=200 + i, name='master', type='branch')])


class EnvironmentTests(unittest.TestCase):
    def setUp(self):
        self.gh = GitHub()
        seed(self.gh)
        self.args = argparse.Namespace(repo='owner/repo', environment='Production',
            approval_reviewer='owner', state_dir=Path('/unused'))

    def verify(self):
        g.Environments(self.args, self.gh.run).verify()

    def test_applied_environments_are_read_only(self):
        self.verify()
        self.assertEqual(self.gh.writes(), [])

    def test_missing_environment_requires_terraform(self):
        self.gh.envs.pop('Production')
        with self.assertRaisesRegex(g.Error, 'infra/live-github'):
            self.verify()
        self.assertEqual(self.gh.writes(), [])

    def test_drift_requires_terraform_not_python_reconciliation(self):
        for name in ('Production', 'production-approval'):
            with self.subTest(name=name):
                seed(self.gh)
                self.gh.envs[name]['config']['can_admins_bypass'] = True
                with self.assertRaises(g.Error):
                    self.verify()
                self.assertEqual(self.gh.writes(), [])

    def test_no_second_reviewer_gate_allowed_on_credential_environment(self):
        self.gh.envs['Production']['config']['reviewers'] = [dict(type='User', id=7)]
        with self.assertRaises(g.Error):
            self.verify()
        self.assertEqual(self.gh.writes(), [])

    def test_extra_branch_policy_fails_closed(self):
        self.gh.envs['Production']['branches'].append(dict(id=999, name='*', type='tag'))
        with self.assertRaises(g.Error):
            self.verify()
        self.assertEqual(self.gh.writes(), [])

    def test_custom_protection_rules_are_not_overwritten(self):
        self.gh.custom = [dict(id=1)]
        with self.assertRaises(g.Error):
            self.verify()
        self.assertEqual(self.gh.writes(), [])

    def test_terraform_owns_mutation(self):
        source = Path(__file__).with_name('live-github-environments.py').read_text()
        self.assertNotIn("'PUT'", source)
        self.assertNotIn("'POST'", source)
        self.assertNotIn("'DELETE'", source)
        self.assertFalse(hasattr(g.Environments, 'execute'))
        self.assertFalse(hasattr(g.Environments, 'plan'))


if __name__ == '__main__':
    unittest.main()
