#!/usr/bin/env python3
"""Behavioral regressions for minimal read-only ownership evidence."""
import copy
import importlib.util
import json
from pathlib import Path
import unittest
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location('mounts', Path(__file__).with_name('inspect-live-mounts.py'))
M = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(M)
MACHINE = '8d99733c27d018'
OLD = 'vol_aaaaaaaaaaaaaa'
LIVE = 'vol_bbbbbbbbbbbbbb'


def values(resource_id, mount_id):
    return {'root_module': {'resources': [
        {'address': 'fly_machine.live', 'values': {'id': MACHINE, 'app': 'test-app',
         'env': {'TOKEN': 'private-env-value'}, 'mount': [{'path': '/data', 'volume': mount_id}]}},
        {'address': 'fly_volume.authority', 'values': {'id': resource_id, 'app': 'test-app',
         'attached_machine_id': MACHINE if resource_id == LIVE else '',
         'readopt_on_host_migration': True, 'private': 'private-provider-value'}}]}}


class InspectionTests(unittest.TestCase):
    def setUp(self):
        self.state = {'format_version': '1.0', 'values': values(OLD, OLD)}
        self.plan = {'format_version': '1.2', 'prior_state': {'values': values(OLD, LIVE)}, 'planned_values': values(OLD, OLD)}
        self.calls = []

    def get(self, path, token):
        self.calls.append(path)
        self.assertEqual(token, 'private-auth-token')
        if path.endswith('/machines/' + MACHINE):
            return {'id': MACHINE, 'region': 'iad', 'config': {
                'env': {'SECRET': 'private-live-config'}, 'mounts': [{'path': '/data', 'volume': LIVE}]}}
        self.assertEqual(path, 'apps/test-app/volumes')
        return [{'id': OLD, 'name': 'live_authority', 'region': 'iad', 'attached_machine_id': ''},
                {'id': LIVE, 'name': 'live_authority', 'region': 'iad', 'attached_machine_id': MACHINE},
                {'id': 'vol_cccccccccccccc', 'name': 'private-unrelated-volume'}]

    def test_distinguishes_persisted_refreshed_and_planned_ownership_without_secrets(self):
        result = M.report(self.state, self.plan, 'test-app', 'private-auth-token', self.get)
        self.assertFalse(result['snapshots']['planned']['resource_matches_live_mount'])
        self.assertTrue(result['snapshots']['refreshed']['mount_matches_live_mount'])
        self.assertEqual(result['snapshots']['persisted']['mounted_volume_id'], OLD)
        self.assertEqual(result['live_mounted_volume_id'], LIVE)
        self.assertEqual(len(result['volume_inventory']), 2)
        self.assertNotIn('private-', json.dumps(result))
        self.assertEqual(len(self.calls), 2)

    def test_reconciled_plan_is_reported_not_applied(self):
        self.plan['planned_values'] = values(LIVE, LIVE)
        result = M.report(self.state, self.plan, 'test-app', 'private-auth-token', self.get)
        self.assertTrue(result['snapshots']['planned']['resource_matches_live_mount'])
        self.assertEqual(result['operation'], 'read_only_no_recovery_performed')

    def test_unknown_invalid_and_incomplete_plans_fail_closed_before_api(self):
        for value in (None, '::error::private', 'vol_unsafe/../../other'):
            plan = copy.deepcopy(self.plan)
            plan['planned_values'] = values(value, LIVE)
            with self.assertRaises(M.InspectionError) as caught:
                M.report(self.state, plan, 'test-app', 'private-auth-token', self.get)
            self.assertNotIn('private', str(caught.exception))
        plan = dict(self.plan, complete=False)
        with self.assertRaisesRegex(M.InspectionError, 'Incomplete'):
            M.report(self.state, plan, 'test-app', 'private-auth-token', self.get)
        self.assertFalse(self.calls)

    def test_unsupported_schema_is_not_reinterpreted(self):
        for version in (None, '2.0', 'private-value'):
            state = dict(self.state, format_version=version)
            with self.assertRaisesRegex(M.InspectionError, 'format version'):
                M.report(state, self.plan, 'test-app', 'private-auth-token', self.get)
        self.assertFalse(self.calls)

    def test_ambiguous_live_mount_never_becomes_recovery_authority(self):
        def get(path, token):
            result = self.get(path, token)
            if isinstance(result, dict):
                result['config']['mounts'].append({'path': '/other', 'volume': OLD})
            return result
        with self.assertRaisesRegex(M.InspectionError, 'ambiguous'):
            M.report(self.state, self.plan, 'test-app', 'private-auth-token', get)

    def test_api_uses_only_get_and_does_not_follow_redirects(self):
        response = unittest.mock.MagicMock()
        response.__enter__.return_value.read.return_value = b'{"id":"safe"}'
        opener = unittest.mock.Mock()
        opener.open.return_value = response
        with patch.object(M.urllib.request, 'build_opener', return_value=opener) as build:
            self.assertEqual(M.api_get('apps/test-app/volumes', 'private-auth-token'), {'id': 'safe'})
        self.assertIs(build.call_args.args[0], M.NoRedirect)
        self.assertEqual(opener.open.call_args.args[0].get_method(), 'GET')
        self.assertIsNone(M.NoRedirect().redirect_request(None, None, 302, '', {}, 'https://other.test'))


if __name__ == '__main__':
    unittest.main()
