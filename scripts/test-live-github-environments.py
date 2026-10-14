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


class EnvironmentTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.args = argparse.Namespace(repo='owner/repo', environment='Production', approval_reviewer='owner',
            approve_environment_changes=False, state_dir=Path(self.temp.name), state_operation=OP,
            bootstrap_state=True, state_mode='plan', dry_run=False, input_env=False, only=['LIVE_HOSTNAME'],
            rotate=False, zone='bmux.dev', hostname=None, pages_project='bmux', publisher_key_file=None,
            state_bucket=None, state_account_id=None, state_region='auto', approve_import=False,
            state_completion_file=None, accept_cost=True)
        self.gh = GitHub()
        g.local.private_dir(self.args.state_dir / 'github-environments')
        self.output = contextlib.redirect_stdout(io.StringIO())
        self.output.__enter__()
        self.addCleanup(self.output.__exit__, None, None, None)

    def manager(self):
        return g.Environments(self.args, self.gh.run)

    def plan(self):
        self.manager().plan(OP)

    def review_apply(self):
        self.manager().execute(OP, 'review')
        self.manager().execute(OP, 'apply')

    def seeded(self):
        self.plan()
        self.review_apply()
        self.gh.calls.clear()
        (self.args.state_dir / 'github-environments' / (OP + '.json')).unlink()

    def test_missing_plan_review_read_only_apply_and_rerun_noop(self):
        self.plan()
        self.manager().execute(OP, 'review')
        self.assertEqual(self.gh.writes(), [])
        self.manager().execute(OP, 'apply')
        self.assertEqual(set(self.gh.envs), {'Production', 'production-approval'})
        for name, value in self.gh.envs.items():
            self.assertEqual([dict(name=v['name'], type=v['type']) for v in value['branches']], [dict(name='master', type='branch')])
            self.assertFalse(value['config']['can_admins_bypass'])
            self.assertFalse(value['config']['prevent_self_review'])
            self.assertEqual(value['config']['reviewers'], [dict(type='User', id=7)] if name == 'production-approval' else [])
        self.gh.calls.clear()
        self.manager().execute(OP, 'apply')
        self.manager().execute(OP, 'reconcile')
        self.assertEqual(self.gh.writes(), [])

    def test_existing_conflict_requires_approval_and_preserves_wait_timer(self):
        self.seeded()
        value = self.gh.envs['production-approval']
        value['config'].update(can_admins_bypass=True, prevent_self_review=True, wait_timer=12)
        value['branches'].append(dict(id=666, name='*', type='tag'))
        self.plan()
        self.manager().execute(OP, 'review')
        with self.assertRaises(g.Error):
            self.manager().execute(OP, 'apply')
        self.assertEqual(self.gh.writes(), [])
        self.args.approve_environment_changes = True
        self.manager().execute(OP, 'apply')
        self.assertEqual(self.gh.envs['production-approval']['config']['wait_timer'], 12)
        self.assertEqual(len(self.gh.envs['production-approval']['branches']), 1)

    def test_unreviewed_and_stale_plans_never_mutate(self):
        self.plan()
        with self.assertRaises(g.Error):
            self.manager().execute(OP, 'apply')
        self.manager().execute(OP, 'review')
        self.gh.envs['Production'] = dict(id=4, config=self.manager().target('Production', None), branches=[])
        with self.assertRaises(g.Error):
            self.manager().execute(OP, 'apply')
        self.assertEqual(self.gh.writes(), [])

    def test_ambiguous_write_resume_after_exact_effect(self):
        self.plan()
        self.manager().execute(OP, 'review')
        self.gh.fail_after_write = True
        with self.assertRaises(b.BootstrapError):
            self.manager().execute(OP, 'apply')
        self.assertEqual(set(self.gh.envs), {'Production'})
        self.manager().execute(OP, 'apply')
        puts = [cmd for cmd, _ in self.gh.writes() if 'PUT' in cmd]
        self.assertEqual(len(puts), 2)

    def test_ambiguous_no_effect_can_explicitly_resume(self):
        self.plan()
        self.manager().execute(OP, 'review')
        self.gh.fail_before_write = True
        with self.assertRaises(b.BootstrapError):
            self.manager().execute(OP, 'apply')
        self.assertEqual(self.gh.envs, {})
        self.manager().execute(OP, 'apply')
        self.assertEqual(len(self.gh.envs), 2)

    def test_custom_protections_and_unknown_state_fail_closed(self):
        self.seeded()
        self.gh.custom = [dict(id=2)]
        with self.assertRaises(g.Error):
            self.plan()
        self.assertEqual(self.gh.writes(), [])
        self.gh.custom = []
        self.gh.envs['Production']['config']['deployment_branch_policy'] = dict(unknown=True)
        with self.assertRaises(g.Error):
            self.plan()

    def test_corrupt_and_wrong_operation_reviewer_or_implementation_rejected(self):
        self.plan()
        self.manager().execute(OP, 'review')
        with self.assertRaises(g.Error):
            self.manager().execute('../outside', 'apply')
        with patch.object(g.local, 'digest', return_value='changed'), self.assertRaises(g.Error):
            self.manager().execute(OP, 'apply')
        path = self.args.state_dir / 'github-environments' / (OP + '.json')
        ledger = g.local.private_read(path)
        ledger['schema'] = 99
        g.local.save(path, ledger)
        with self.assertRaises(g.Error):
            self.manager().execute(OP, 'apply')
        self.assertEqual(self.gh.writes(), [])

    def test_api_errors_do_not_mean_missing_and_inventory_must_be_complete(self):
        def broken(*args, **kwargs):
            raise b.BootstrapError('permission denied')
        with self.assertRaises(b.BootstrapError):
            g.Environments(self.args, broken)
        with self.assertRaises(g.Error):
            g.pages(lambda *a: '{"total_count": 1, "environments": []}', 'endpoint', 'environments')
        with self.assertRaises(g.Error):
            g.pages(lambda *a: '{"environments": []}', 'endpoint', 'environments')

    def test_capitalization_collision_and_shared_approval_name_rejected(self):
        self.gh.envs['production'] = {}
        with self.assertRaises(g.Error):
            self.plan()
        self.args.environment = 'production-approval'
        with self.assertRaises(g.Error):
            self.plan()
        self.assertEqual(self.gh.writes(), [])

    def test_new_operation_on_matching_environments_and_existing_secrets_is_noop(self):
        self.seeded()
        self.gh.secrets['LIVE_HOSTNAME'] = 'existing-secret-value'
        self.plan()
        self.review_apply()
        self.assertEqual(self.gh.writes(), [])
        self.assertEqual(self.gh.secrets, {'LIVE_HOSTNAME': 'existing-secret-value'})

    def test_drift_after_ambiguous_write_is_not_accepted(self):
        self.plan()
        self.manager().execute(OP, 'review')
        self.gh.fail_after_write = True
        with self.assertRaises(b.BootstrapError):
            self.manager().execute(OP, 'apply')
        self.gh.envs['Production']['config']['can_admins_bypass'] = True
        writes = len(self.gh.writes())
        with self.assertRaises(g.Error):
            self.manager().execute(OP, 'apply')
        self.assertEqual(len(self.gh.writes()), writes)

    def test_partial_post_and_delete_resume_without_duplicate_effect(self):
        self.seeded()
        self.gh.envs['Production']['branches'] = [dict(id=42, name='*', type='branch')]
        self.args.approve_environment_changes = True
        self.plan()
        self.manager().execute(OP, 'review')
        original = self.gh.run
        fail_methods = ['POST', 'DELETE']
        def run(cmd, body=None):
            if '--method' in cmd and cmd[cmd.index('--method') + 1] in fail_methods:
                fail_methods.remove(cmd[cmd.index('--method') + 1])
                self.gh.fail_after_write = True
            return original(cmd, body)
        for _ in range(2):
            with self.assertRaises(b.BootstrapError):
                g.Environments(self.args, run).execute(OP, 'apply')
        g.Environments(self.args, run).execute(OP, 'apply')
        self.assertEqual(len([cmd for cmd, _ in self.gh.writes() if 'POST' in cmd]), 1)
        self.assertEqual(len([cmd for cmd, _ in self.gh.writes() if 'DELETE' in cmd]), 1)

    def test_binding_cannot_be_dropped_on_later_apply(self):
        self.plan()
        self.args.approval_reviewer = None
        self.args.state_mode = 'apply'
        self.args.state_account_id = 'a' * 32
        with self.assertRaises(b.BootstrapError):
            b.state_bootstrap(self.args, lambda *a: self.fail('provider request'), ['LIVE_STATE_BUCKET'], {})

    def test_provider_free_dryrun(self):
        self.args.dry_run = True
        b.provision(self.args, lambda *a: self.fail('provider request'))

    def test_bootstrap_gates_backend_and_secret_writes_on_environment_verification(self):
        backend_calls = []
        backend = dict(bucket='bmux-opentofu-state', region='auto', key='bmux/live/production/terraform.tfstate',
                       endpoint='https://' + 'a' * 32 + '.r2.cloudflarestorage.com', use_lockfile=True, encrypt=True)
        def run(cmd, body=None):
            if len(cmd) > 1 and cmd[1].endswith('provision-live-state.py'):
                backend_calls.append(cmd)
                return json.dumps(dict(schema=2, operation=OP, phase='complete' if '--apply' in cmd else 'planned',
                                       backend=backend, resources=[]))
            return self.gh.run(cmd, body)
        def invoke():
            b.provision(self.args, run, {}, cloudflare=object())
        with patch.object(b, 'discover', return_value=('a' * 32, 'b' * 32)):
            invoke()
            self.assertEqual(self.gh.writes(), [])
            self.args.state_mode = 'review'
            invoke()
            self.assertEqual(self.gh.writes(), [])
            self.args.state_mode = 'apply'
            self.gh.fail_after_write = True
            with self.assertRaises(b.BootstrapError):
                invoke()
            self.assertFalse(any('--apply' in cmd for cmd in backend_calls))
            self.assertEqual(self.gh.secrets, {})
            invoke()
            self.assertTrue(any('--apply' in cmd for cmd in backend_calls))
            self.assertEqual(self.gh.secrets, {'LIVE_HOSTNAME': 'bmux.dev'})
            self.gh.calls.clear()
            invoke()
            self.assertEqual(self.gh.writes(), [])


if __name__ == '__main__':
    unittest.main()
