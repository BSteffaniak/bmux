#!/usr/bin/env python3
"""Offline R2 provisioning contract tests; no provider calls."""
import contextlib
import importlib.util
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

HERE = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location('provision', HERE / 'provision-live-state.py')
p = importlib.util.module_from_spec(spec)
spec.loader.exec_module(p)
ACCOUNT = 'a' * 32
C = dict(repo='owner/repo', bucket='bmux-opentofu-state', region='auto', account_id=ACCOUNT)


class CF:
    def __init__(self, row=None):
        self.row = row
        self.calls = []

    def request(self, method, path, body=None):
        self.calls.append((method, path, body))
        if method == 'GET':
            return {'result': {'buckets': [] if self.row is None else [self.row]}}
        self.row = dict(name=body['name'], storage_class='Standard', creation_date='2026-10-14T00:00:00Z')
        return {'result': self.row}


class ProvisionTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.cf = CF()

    def tearDown(self):
        self.temp.cleanup()

    def obj(self):
        return p.Provision(C, self.root, self.cf)

    def test_exact_create_and_resume(self):
        obj = self.obj()
        result = obj.plan()
        op = result['operation']
        with self.assertRaises(p.Error):
            obj.apply('b' * 32, True)
        with self.assertRaises(p.Error):
            obj.apply(op, False)
        done = obj.apply(op, True)
        self.assertEqual(done['phase'], 'complete')
        resumed = self.obj()
        resumed.exact(op)
        self.assertEqual(resumed.result(), done)
        self.assertEqual(sum(call[0] == 'POST' for call in self.cf.calls), 1)
        self.assertNotIn('role_arn', json.dumps(done))

    def test_inventory_change_rejects_before_post(self):
        obj = self.obj()
        op = obj.plan()['operation']
        self.cf.row = dict(name=C['bucket'], storage_class='Standard', creation_date='different')
        with self.assertRaises(p.Error):
            obj.apply(op, True)
        self.assertFalse(any(c[0] == 'POST' for c in self.cf.calls))

    def test_reuse_requires_explicit_adoption_and_does_not_modify_policy(self):
        self.cf.row = dict(name=C['bucket'], storage_class='Standard', creation_date='date')
        with self.assertRaises(p.Error):
            self.obj().plan()
        obj = self.obj()
        op = obj.plan(True)['operation']
        obj.apply(op, True)
        self.assertTrue(all(c[0] == 'GET' for c in self.cf.calls))

    def test_legacy_corrupt_and_choices_fence(self):
        p.local.save(self.root / 'operation.json', {'schema': 1})
        with self.assertRaises(p.Error):
            self.obj()
        p.local.save(self.root / 'operation.json', {'schema': 2})
        with self.assertRaises(p.Error):
            self.obj()

    def test_interrupt_never_adopts_by_name(self):
        obj = self.obj()
        op = obj.plan()['operation']
        obj.save('applying')
        self.cf.row = dict(name=C['bucket'], storage_class='Standard', creation_date='unknown')
        with self.assertRaises(p.Error):
            self.obj().reconcile(op)

    def test_real_cli_provider_free(self):
        env = {'PATH': os.environ['PATH'], 'HOME': str(self.root), 'PYTHONDONTWRITEBYTECODE': '1'}
        cmd = [sys.executable, str(HERE / 'provision-live-state.py'), '--repo', C['repo'], '--account-id', ACCOUNT]
        result = subprocess.run(cmd, env=env, capture_output=True, text=True, timeout=10)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(result.stdout)['phase'], 'dryrun')
        for extra in (['--role', 'bad'], ['--region', 'us-east-1']):
            result = subprocess.run(cmd + extra, env=env, capture_output=True, text=True, timeout=10)
            self.assertNotEqual(result.returncode, 0)

    def test_symlink_ledger_reject(self):
        file = self.root / 'other.json'
        file.write_text('{}')
        (self.root / 'operation.json').symlink_to(file)
        with self.assertRaises(OSError):
            self.obj()

    def test_private_ledger_no_secret(self):
        obj = self.obj()
        obj.plan()
        path = self.root / 'operation.json'
        self.assertEqual(path.stat().st_mode & 0o777, 0o600)
        self.assertNotIn('token', path.read_text())
        self.assertNotIn('secret', path.read_text())

    def test_completion_exact_private_and_new_only(self):
        obj = self.obj()
        op = obj.plan()['operation']
        file = self.root / 'completion.json'
        with self.assertRaises(p.Error):
            obj.completion(op, file)
        obj.apply(op, True)
        value = obj.completion(op, file)
        binding = value.pop('binding')
        self.assertEqual(binding, p.local.identity(value))
        self.assertEqual(file.stat().st_mode & 0o777, 0o600)
        with self.assertRaises(p.Error):
            obj.completion(op, file)
        other = self.root / 'other'
        other.mkdir(mode=0o700)
        reused = p.Provision(C, other, self.cf)
        reuse_op = reused.plan(True)['operation']
        reused.apply(reuse_op, True)
        with self.assertRaises(p.Error):
            reused.completion(reuse_op, other / 'completion.json')

    def test_inventory_unknown_reject(self):
        self.cf.row = {'name': C['bucket']}
        with self.assertRaises(p.Error):
            self.obj().plan(True)


if __name__ == '__main__':
    unittest.main()
