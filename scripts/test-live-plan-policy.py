#!/usr/bin/env python3
"""Offline policy regressions for the Fly-only end state."""
import importlib.util
from pathlib import Path
import json
import subprocess
import sys
import tempfile
import unittest

spec = importlib.util.spec_from_file_location('policy', Path(__file__).with_name('live-plan-policy.py'))
m = importlib.util.module_from_spec(spec)
spec.loader.exec_module(m)


def plan(address, actions, **values):
    return {'resource_changes': [{'address': address, 'change': {'actions': actions, **values}}]}


class PolicyTests(unittest.TestCase):
    def test_image_only_update(self):
        before = {'image': 'old', 'region': 'iad', 'env': {'key': 'digest'}}
        after = dict(before, image='new')
        self.assertTrue(m.validate(plan('fly_machine.live', ['update'], before=before, after=after)))
        after['region'] = 'lax'
        with self.assertRaises(ValueError): m.validate(plan('fly_machine.live', ['update'], before=before, after=after))

    def test_configure_only_non_destructive_known_resources(self):
        for address in m.CONFIGURE_RESOURCES:
            self.assertTrue(m.validate(plan(address, ['create']), configure=True))
            with self.assertRaises(ValueError): m.validate(plan(address, ['delete']), configure=True)
        with self.assertRaises(ValueError): m.validate(plan('other.resource', ['create']), configure=True)

    def test_legacy_worker_mutations_are_never_admitted(self):
        for address in ('cloudflare_worker.live', 'cloudflare_worker_version.live',
                        'cloudflare_workers_deployment.live', 'cloudflare_workers_route.live'):
            for actions in (['no-op'], ['create'], ['update'], ['delete']):
                for configure in (False, True):
                    with self.assertRaises(ValueError):
                        m.validate(plan(address, actions), configure=configure)

    def test_unknown_plan_and_imports_fail_closed(self):
        for extra in ({'errored': True}, {'complete': False}, {'deferred_changes': [{}]}):
            with self.assertRaises(ValueError):
                m.validate(dict(resource_changes=[], **extra))
        candidate = plan('fly_machine.live', ['no-op'], importing={'id': 'unexpected'})
        with self.assertRaises(ValueError):
            m.validate(candidate)
        self.assertTrue(m.validate(candidate, configure=True))

    def test_cli_reports_only_fixed_policy_reason(self):
        secret = 'PRIVATE_PROVIDER_VALUE_DO_NOT_LOG'
        candidates = [
            (plan('fly_machine.live', ['update'],
                  before={'image': 'old', 'env': {'token': secret}},
                  after={'image': 'new', 'env': {'token': 'changed'}}),
             'Machine infrastructure drift detected'),
            (plan(secret, ['create']), 'Unexpected resource or destructive change'),
        ]
        for candidate, reason in candidates:
            with tempfile.TemporaryDirectory() as directory:
                path = Path(directory) / 'plan.json'
                path.write_text(json.dumps(candidate))
                result = subprocess.run([sys.executable, str(Path(m.__file__)), str(path)],
                                        capture_output=True, text=True)
            self.assertEqual(result.returncode, 2)
            self.assertIn(reason, result.stderr)
            self.assertNotIn(secret, result.stdout + result.stderr)

    def test_cli_never_prints_parser_errors_or_plan_contents(self):
        secret = 'PRIVATE_PROVIDER_VALUE_DO_NOT_LOG'
        for content in ('{"' + secret + '":', '{"resource_changes": ["' + secret + '"]}'):
            with tempfile.TemporaryDirectory() as directory:
                path = Path(directory) / 'plan.json'
                path.write_text(content)
                result = subprocess.run([sys.executable, str(Path(m.__file__)), str(path)],
                                        capture_output=True, text=True)
            self.assertEqual(result.returncode, 2)
            self.assertIn('invalid or unreadable plan', result.stderr)
            self.assertNotIn(secret, result.stdout + result.stderr)

    def test_drift_inventory_reports_all_known_changes_without_values(self):
        secret = 'PRIVATE_PROVIDER_VALUE_DO_NOT_LOG'
        candidate = {'resource_changes': [
            *plan('cloudflare_dns_record.relay_acme', ['update'],
                  before={'content': secret}, after={'content': 'different'})['resource_changes'],
            *plan('cloudflare_pages_domain.live', ['create'])['resource_changes'],
            *plan('fly_machine.live', ['update'], before={'image': secret},
                  after={'image': 'new'})['resource_changes'],
            *plan(secret + '\\n::error::injected', ['delete'])['resource_changes'],
            *plan('cloudflare_r2_bucket.state', ['no-op'])['resource_changes'],
        ]}
        summary = m.public_change_summary(candidate)
        self.assertIn('cloudflare_dns_record.relay_acme: update', summary)
        self.assertIn('cloudflare_pages_domain.live: create', summary)
        self.assertIn('fly_machine.live: update', summary)
        self.assertIn('unrecognized resource: delete', summary)
        self.assertNotIn('cloudflare_r2_bucket.state', summary)
        self.assertNotIn(secret, summary)
        self.assertNotIn('::error::', summary)
        self.assertNotIn('content', summary)
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'plan.json'
            path.write_text(json.dumps(candidate))
            result = subprocess.run([sys.executable, str(Path(m.__file__)), str(path)],
                                    capture_output=True, text=True)
        self.assertEqual(result.returncode, 2)
        self.assertIn('configure first', result.stderr)
        self.assertIn(summary, result.stderr)
        self.assertNotIn(secret, result.stdout + result.stderr)

    def test_drift_inventory_is_bounded_fixed_vocabulary_and_handles_malformed_input(self):
        for candidate in (None, [], {'resource_changes': 'private'}):
            self.assertEqual(m.public_change_summary(candidate), '')
        candidate = {'resource_changes': [None, {'address': 'private', 'change': 'private'},
                     {'address': 'private', 'change': {'actions': ['private']}}] * 1000}
        self.assertEqual(m.public_change_summary(candidate),
                         '  unrecognized resource: unsupported action')
        for actions in (['create', 'delete'], ['delete', 'create']):
            self.assertEqual(m.public_change_summary(plan('fly_machine.live', actions)),
                             '  fly_machine.live: replace')

    def test_no_release_create_or_replacement(self):
        for actions in [['create'], ['delete'], ['delete', 'create'], ['create', 'delete']]:
            with self.assertRaises(ValueError): m.validate(plan('fly_machine.live', actions))


if __name__ == '__main__': unittest.main()
