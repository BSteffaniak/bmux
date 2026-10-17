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
