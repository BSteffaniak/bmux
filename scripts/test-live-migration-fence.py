#!/usr/bin/env python3
"""Hermetic migration fencing tests. No provider, OpenTofu, or network calls."""
import hashlib
import importlib.util
import json
import io
import os
from pathlib import Path
import subprocess
import sys
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location('archive_tests', Path(__file__).with_name('test-archive-live-state.py'))
fixtures = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixtures)
m = fixtures.m
BODY = fixtures.BODY
DIGEST = 'a' * 64
SOURCE = hashlib.sha256(BODY).hexdigest()
ENV = {'LIVE_ENVIRONMENT': 'production', 'LIVE_STATE_BUCKET': 'offline-state',
       'CLOUDFLARE_ACCOUNT_ID': '1' * 32}


class FakeR2(fixtures.Memory):
    def __init__(self):
        super().__init__()
        self.config = dict(key=self.key, bucket=ENV['LIVE_STATE_BUCKET'], region='auto', use_lockfile=True,
                           endpoint='https://' + ENV['CLOUDFLARE_ACCOUNT_ID'] + '.r2.cloudflarestorage.com')
        self.hook = None

    def call(self, operation, *args):
        if self.hook:
            self.hook(operation, args)
        return super().call(operation, *args)


class FenceTests(unittest.TestCase):
    def setUp(self):
        self.env = patch.dict(os.environ, ENV)
        self.env.start()
        self.store = FakeR2()
        self.addCleanup(self.env.stop)
        self.addCleanup(self.store.temp.cleanup)

    def begin(self):
        return self.store.migration_begin(DIGEST, SOURCE)

    def receipt_file(self, receipt):
        path = self.store.directory / 'receipt.json'
        if path.exists():
            path.unlink()
        m.save_private(path, receipt)
        return path

    def mutations(self):
        return [(op, args) for op, args in self.store.calls if op in ('put-object', 'delete-object')]

    def test_check_stable_read_only_hash_and_archive_shape(self):
        result = self.store.migration_check()
        self.assertEqual(result, dict(state_etag=self.store.etag(BODY), sha256=SOURCE))
        self.assertFalse(self.mutations())
        self.assertEqual(self.store.archive()['sha256'], result['sha256'])

    def test_full_lifecycle_preserves_single_authority_and_blocks_restart(self):
        receipt = self.begin()
        key = self.store.migration_key()
        marker = json.loads(self.store.objects[key])
        self.assertEqual(set(marker), {'schema', 'phase', 'operation', 'migration_digest', 'source_sha256'})
        self.assertNotIn('etag', marker)
        self.assertEqual(marker['phase'], 'applying')
        self.assertNotIn(self.store.admission_key(), self.store.objects)
        path = self.receipt_file(receipt)
        # An apply changes encrypted state; completing must not require its old body or plaintext.
        current = BODY.replace(b'"serial": 1', b'"serial": 2')
        self.store.objects[self.store.key] = current
        completed = self.store.migration_complete(path)
        self.assertEqual(completed['status'], 'completed')
        self.assertEqual(self.store.objects[self.store.key], current)
        self.assertEqual([args[args.index('--key') + 1] for _, args in self.mutations()], [key, key])
        self.assertIn('--if-none-match', self.mutations()[0][1])
        self.assertIn('--if-match', self.mutations()[1][1])
        restarted = FakeR2()
        self.addCleanup(restarted.temp.cleanup)
        restarted.objects = self.store.objects.copy()
        with self.assertRaisesRegex(m.MigrationError, 'migration_existing_completed'):
            restarted.migration_check()
        self.assertEqual(restarted.migration_complete(path), completed)
        self.assertFalse(any(op == 'put-object' for op, _ in restarted.calls))

    def test_same_identity_rejects_replay_conflicts_reject(self):
        self.begin()
        with self.assertRaisesRegex(m.MigrationError, 'migration_existing_applying'):
            self.begin()
        with self.assertRaisesRegex(m.MigrationError, 'migration_identity_conflict'):
            self.store.migration_begin('b' * 64, SOURCE)
        with self.assertRaisesRegex(m.MigrationError, 'migration_identity_conflict'):
            self.store.migration_begin(DIGEST, 'b' * 64)
        with self.assertRaisesRegex(m.MigrationError, 'migration_existing_applying'):
            self.store.migration_check()
        self.assertEqual(len(self.mutations()), 1)

    def test_racing_begin_conditional_put_only_one_winner(self):
        key = self.store.migration_key()
        def race(op, args):
            if op == 'put-object' and key in args:
                self.store.hook = None
                # A second actor wins after all reads but before our conditional PUT.
                self.store.migration_begin('b' * 64, SOURCE)
        self.store.hook = race
        with self.assertRaises(m.StateError):
            self.begin()
        self.assertEqual(json.loads(self.store.objects[key])['migration_digest'], 'b' * 64)
        self.assertEqual(len(self.mutations()), 2)

    def test_racing_same_identity_begin_does_not_replay(self):
        key = self.store.migration_key()
        def race(op, args):
            if op == 'put-object' and key in args:
                self.store.hook = None
                self.begin()
        self.store.hook = race
        with self.assertRaises(m.StateError):
            self.begin()
        with self.assertRaisesRegex(m.MigrationError, 'migration_existing_applying'):
            self.begin()
        self.assertEqual(json.loads(self.store.objects[key])['migration_digest'], DIGEST)
        self.assertEqual(len(self.mutations()), 2)  # Winner and rejected CAS, never replayed effects.

    def test_check_racing_begin_detected_on_final_fence_read(self):
        count = 0
        def race(op, args):
            nonlocal count
            if op == 'get-object' and self.store.key in args:
                count += 1
                if count == 2:
                    self.store.hook = None
                    self.begin()
        self.store.hook = race
        with self.assertRaisesRegex(m.MigrationError, 'migration_existing_applying'):
            self.store.migration_check()

    def test_source_hash_and_etag_and_locks_fail_closed(self):
        with self.assertRaisesRegex(m.MigrationError, 'migration_source_changed'):
            self.store.migration_begin(DIGEST, '0' * 64)
        self.store.objects[self.store.key + '.tflock'] = b'lock'
        with self.assertRaises(m.StateError):
            self.begin()
        self.assertFalse(self.mutations())

    def test_state_change_during_stable_reads_rejected(self):
        count = 0
        def race(op, args):
            nonlocal count
            if op == 'get-object' and self.store.key in args:
                count += 1
                if count == 2:
                    self.store.objects[self.store.key] = BODY + b' '
        self.store.hook = race
        with self.assertRaises(m.StateError):
            self.begin()
        self.assertFalse(self.mutations())

    def test_begin_ack_loss_leaves_applying_fence_no_retry(self):
        original = self.store.put
        def lost(*args):
            original(*args)
            raise m.StateError('private provider response must not leak')
        with patch.object(self.store, 'put', side_effect=lost):
            with self.assertRaises(m.StateError):
                self.begin()
        self.assertEqual(len(self.mutations()), 1)
        with self.assertRaisesRegex(m.MigrationError, 'migration_existing_applying'):
            self.begin()

    def test_completion_ack_loss_exact_original_receipt_terminal_retry(self):
        path = self.receipt_file(self.begin())
        original = self.store.put
        def lost(*args):
            original(*args)
            raise m.StateError('ack lost')
        with patch.object(self.store, 'put', side_effect=lost):
            with self.assertRaises(m.StateError):
                self.store.migration_complete(path)
        count = len(self.mutations())
        first = self.store.migration_complete(path)
        self.assertEqual(first, self.store.migration_complete(path))
        self.assertEqual(len(self.mutations()), count)
        self.assertEqual(first['status'], 'completed')

    def test_bad_receipt_bindings_rejected_applying_and_completed(self):
        receipt = self.begin()
        for completed in (False, True):
            if completed:
                self.store.migration_complete(self.receipt_file(receipt))
            for field, value in (('source_sha256', 'b' * 64), ('migration_digest', 'c' * 64),
                                 ('etag', '"' + '0' * 64 + '"'), ('operation', '00000000-0000-0000-0000-000000000000'),
                                 ('key', self.store.key), ('authority', '0' * 64), ('binding', '0' * 64)):
                with self.subTest(completed=completed, field=field):
                    changed = dict(receipt, **{field: value})
                    # Even a self-consistent local receipt cannot override the remote identity.
                    if field != 'binding':
                        changed['binding'] = m.migration_binding({k: v for k, v in changed.items() if k != 'binding'})
                    before = len(self.mutations())
                    with self.assertRaises(m.StateError):
                        self.store.migration_complete(self.receipt_file(changed))
                    self.assertEqual(len(self.mutations()), before)

    def test_completion_cas_conflict_keeps_fence(self):
        path = self.receipt_file(self.begin())
        key = self.store.migration_key()
        def race(op, args):
            if op == 'put-object' and key in args:
                self.store.objects[key] += b' '
        self.store.hook = race
        with self.assertRaises(m.StateError):
            self.store.migration_complete(path)
        self.assertIn(key, self.store.objects)
        self.assertEqual(json.loads(self.store.objects[key])['phase'], 'applying')

    def test_completion_requires_private_absolute_receipt_and_current_encrypted_state(self):
        receipt = self.begin()
        path = self.receipt_file(receipt)
        path.chmod(0o644)
        with self.assertRaises(m.StateError):
            self.store.migration_complete(path)
        path.chmod(0o600)
        with self.assertRaises(m.StateError):
            self.store.migration_complete(Path('receipt.json'))
        for body in (None, b'{"resources": []}'):
            if body is None:
                del self.store.objects[self.store.key]
            else:
                self.store.objects[self.store.key] = body
            with self.assertRaises(m.StateError):
                self.store.migration_complete(path)
        self.assertEqual(len(self.mutations()), 1)

    def test_malformed_marker_etag_and_identity_never_treated_as_absent(self):
        key = self.store.migration_key()
        receipt = self.begin()
        valid = json.loads(self.store.objects[key])
        for raw in (b'null', b'{}', b'{', json.dumps(dict(valid, schema=True)).encode(),
                    json.dumps(dict(valid, source_sha256='secret')).encode(),
                    json.dumps(dict(valid, operation='bad')).encode(), json.dumps(dict(valid, phase='unknown')).encode()):
            self.store.objects[key] = raw
            with self.assertRaises((m.StateError, ValueError)):
                self.store.migration_check()
        self.store.objects[key] = json.dumps(valid).encode()
        with patch.object(self.store, 'etag', return_value='bad'):
            with self.assertRaises(m.StateError):
                self.store.migration_complete(self.receipt_file(receipt))

    def test_digest_validation_production_authority_and_transport_failures(self):
        for value in ('a' * 63, 'A' * 64, 'x' * 64, None, 'a' * 64 + '\n'):
            with self.assertRaises(m.StateError):
                self.store.migration_begin(value, SOURCE)
        with patch.dict(os.environ, LIVE_ENVIRONMENT='staging'):
            with self.assertRaises(m.StateError):
                self.store.migration_check()
        for name, value in (('bucket', 'other'), ('key', 'other'), ('endpoint', 'https://wrong'),
                            ('use_lockfile', False), ('region', 'us-east-1')):
            with patch.dict(self.store.config, {name: value}):
                with self.assertRaises(m.StateError):
                    self.begin()
        with patch.object(self.store, 'call', side_effect=m.StateError('secret provider data')):
            with self.assertRaises(m.StateError):
                self.begin()
        self.assertFalse(self.mutations())

    def test_cli_structured_output_and_private_failure_suppression(self):
        with patch.object(m, 'Store', return_value=self.store), patch.object(sys, 'argv', ['archive-live-state.py', 'migration-check']):
            with patch('sys.stdout', new_callable=io.StringIO) as output:
                m.main()
        self.assertEqual(json.loads(output.getvalue())['sha256'], SOURCE)
        # Missing environment fails before transport construction; no live calls.
        env = {'PATH': os.environ.get('PATH', ''), 'PYTHONDONTWRITEBYTECODE': '1',
               'R2_STATE_SECRET_ACCESS_KEY': 'DO-NOT-LEAK-SECRET', 'CLOUDFLARE_API_TOKEN': 'DO-NOT-LEAK-TOKEN'}
        script = str(Path(__file__).with_name('archive-live-state.py'))
        result = subprocess.run([sys.executable, script, 'migration-check'], env=env,
                                capture_output=True, text=True, timeout=5, check=False)
        self.assertEqual(result.returncode, 2)
        self.assertEqual(result.stdout, '')
        self.assertEqual(json.loads(result.stderr)['status'], 'migration_rejected')
        self.assertNotIn('DO-NOT-LEAK', result.stderr)
        self.assertNotIn('Traceback', result.stderr)

    def test_duplicate_json_fields_and_noncanonical_etags_rejected(self):
        receipt = self.begin()
        key = self.store.migration_key()
        self.store.objects[key] = self.store.objects[key][:-1] + b', "phase":"applying"}'
        with self.assertRaises(m.StateError):
            self.store.migration_check()
        path = self.receipt_file(receipt)
        path.write_text(path.read_text()[:-1] + ', "schema":1}')
        with self.assertRaises(m.StateError):
            self.store.migration_complete(path)
        for value in ('*', 'W/"' + 'a' * 32 + '"', '"bad"', '"' + 'a' * 32 + '"\n', 'secret', None):
            self.assertFalse(m.migration_etag(value))

    def test_completion_locked_rejected_without_mutation(self):
        path = self.receipt_file(self.begin())
        self.store.objects[self.store.key + '.tflock'] = b'lock'
        with self.assertRaises(m.StateError):
            self.store.migration_complete(path)
        self.assertEqual(len(self.mutations()), 1)

    def test_truncated_fence_inventory_and_missing_ack_fail_closed(self):
        with patch.object(self.store, 'call', return_value={'IsTruncated': True}):
            with self.assertRaises(m.StateError):
                self.store.migration_check()
        original = self.store.put
        def missing(*args):
            original(*args)
            return {}
        with patch.object(self.store, 'put', side_effect=missing):
            with self.assertRaisesRegex(m.MigrationError, 'migration_write_ambiguous'):
                self.begin()
        self.assertIn(self.store.migration_key(), self.store.objects)
        self.assertEqual(len(self.mutations()), 1)


if __name__ == '__main__':
    unittest.main()
