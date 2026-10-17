#!/usr/bin/env python3
import importlib.util
import json
from pathlib import Path
import unittest
from unittest.mock import Mock

SPEC = importlib.util.spec_from_file_location('repair', Path(__file__).with_name('repair-live-app.py'))
M = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(M)
BODY = json.dumps({'encryption_version': 'v0', 'meta': {'key': {}},
                   'encrypted_data': 'YWFhYWFhYWFhYWFhYWFhYQ=='}).encode()


class RepairTests(unittest.TestCase):
    def state(self):
        return {'version': 4, 'serial': 3, 'lineage': 'authority', 'resources': [
            {'mode': 'managed', 'type': 'fly_app', 'name': 'live', 'instances': [
                {'status': 'tainted', 'attributes': {'id': 'durable', 'name': 'app', 'org': 'canonical'}}]}]}

    def test_identity_and_configuration(self):
        live = {'name': 'app', 'id': 'durable', 'organization': {'slug': 'canonical'}}
        self.assertTrue(M.verify(self.state(), live, 'app', 'canonical'))
        for key, value in [('id', 'other'), ('name', 'other'), ('organization', {'slug': 'alias'})]:
            with self.assertRaises(ValueError):
                M.verify(self.state(), dict(live, **{key: value}), 'app', 'canonical')
        with self.assertRaises(ValueError):
            M.verify(self.state(), live, 'app', 'alias')

    def store(self):
        store = Mock()
        store.key = 'canonical'
        store.prefix = 'history/canonical/'
        store.put.return_value = {'ETag': '"' + 'a' * 32 + '"'}
        store.get.return_value = (BODY, 'etag')
        store.migration_marker.return_value = 'marker'
        return store

    def test_archive_lock_conditional_write_and_release(self):
        store = self.store()
        M.publish(store, BODY, 'etag', 'marker', BODY)
        calls = store.put.call_args_list
        self.assertEqual(calls[0].args[0], 'canonical.tflock')
        self.assertEqual(calls[0].args[-2:], ('--if-none-match', '*'))
        self.assertTrue(calls[1].args[0].startswith(store.prefix))
        self.assertEqual(calls[2].args, ('canonical', BODY, '--if-match', 'etag'))
        store.call.assert_called_once_with('delete-object', '--key', 'canonical.tflock',
                                          '--if-match', '"' + 'a' * 32 + '"')

    def test_repair_exact_delta_with_writer_version_stamp(self):
        import os
        from unittest.mock import patch
        state = self.state()
        state['terraform_version'] = '1.10.8'
        after = json.loads(json.dumps(state))
        after['serial'] += 1
        after['terraform_version'] = '1.10.9'
        after['resources'][0]['instances'][0].pop('status')
        store = self.store()
        store.migration_marker.return_value = ({'phase': 'applying'}, 'marker-etag')
        calls = []
        def runner(args, **kwargs):
            calls.append(args)
            output = ''
            if args[1:] == ['state', 'pull']:
                output = json.dumps(after if any('untaint' in call for call in calls) else state)
            elif args[1:] == ['version', '-json']:
                output = json.dumps({'terraform_version': '1.10.9'})
            return M.subprocess.CompletedProcess(args, 0, output, '')
        response = Mock()
        response.__enter__ = Mock(return_value=response)
        response.__exit__ = Mock(return_value=False)
        response.read.return_value = json.dumps({'name': 'app', 'id': 'durable',
                                                'organization': {'slug': 'canonical'}}).encode()
        with patch.dict(os.environ, {'LIVE_FLY_APP': 'app', 'LIVE_FLY_ORG': 'canonical',
                                    'FLY_API_TOKEN': 'test', 'TOFU_STATE_ENCRYPTION_PASSPHRASE': 'test'}), \
                patch.object(M.subprocess, 'run', side_effect=runner), \
                patch.object(M.urllib.request, 'urlopen', return_value=response), \
                patch.object(M, 'publish') as publish:
            self.assertTrue(M.repair(store, True)['changed'])
            publish.assert_called_once()
            after['resources'][0]['instances'][0]['attributes']['id'] = 'unexpected'
            publish.reset_mock()
            calls.clear()
            with self.assertRaisesRegex(ValueError, 'Unexpected state mutation'):
                M.repair(store, True)
            publish.assert_not_called()

    def test_delta_diagnostics_never_exposes_state_values_or_unknown_keys(self):
        before = self.state()
        after = json.loads(json.dumps(before))
        after['PRIVATE_KEY_NEVER_PRINT'] = 'SECRET_NEVER_PRINT'
        after['serial'] += 1
        result = M.delta_diagnostics(before, after, before)
        self.assertFalse(result['expected_fields_match']['serial'])
        self.assertFalse(result['other_fields_match'])
        self.assertFalse(result['serial_unchanged'])
        self.assertTrue(result['resources_unchanged'])
        self.assertNotIn('NEVER_PRINT', json.dumps(result))
        self.assertNotIn('durable', json.dumps(result))

    def test_changed_authority_retains_lock(self):
        for change in ('etag', 'marker', 'write'):
            store = self.store()
            if change == 'etag':
                store.get.return_value = (BODY, 'changed')
            elif change == 'marker':
                store.migration_marker.return_value = 'changed'
            else:
                store.put.side_effect = [{'ETag': '"' + 'a' * 32 + '"'}, {}, OSError('ambiguous')]
            with self.assertRaises((ValueError, OSError)):
                M.publish(store, BODY, 'etag', 'marker', BODY)
            store.call.assert_not_called()


if __name__ == '__main__':
    unittest.main()
