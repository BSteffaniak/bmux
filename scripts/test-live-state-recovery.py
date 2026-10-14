#!/usr/bin/env python3
"""Recovery verifies original authority, audit and bucket generation; never provider writes."""
import copy
import importlib.util
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch


def load(name, filename):
    spec = importlib.util.spec_from_file_location(name, Path(__file__).with_name(filename))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


r = load('recovery', 'live-state-recovery.py')
f = load('state_test', 'test-live-state-provision.py')
p = f.p
TIME = '2026-10-14T20:38:01Z'
START = '2026-10-14T20:38:00Z'
AUDIT = '4aa591f0-6e2b-4083-9516-dacb6cc4e684'


class Reader:
    def __init__(self):
        self.calls = []
        self.audit = dict(id=AUDIT, when=TIME, action=dict(type='create', result=True),
                          owner=dict(id=f.ACCOUNT), interface='API', resource=dict(type='r2.bucket', id=f.C['bucket']))
        self.bucket = dict(name=f.C['bucket'], storage_class='Standard', creation_date=TIME)

    def get(self, path, stage):
        self.calls.append((path, stage))
        return copy.deepcopy(dict(result=[self.audit] if '/audit_logs?' in path else self.bucket))


class RecoveryTests(unittest.TestCase):
    def setUp(self):
        import contextlib
        import io
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.cf = f.CF()
        self.obj = p.Provision(f.C, self.root, self.cf)
        self.op = self.obj.plan()['operation']
        self.obj.save('applying')
        import os
        stamp = r.instant(START).timestamp()
        os.utime(self.obj.path, (stamp, stamp))
        self.reader = Reader()
        self.confirm = unittest.mock.Mock()
        redirect = contextlib.redirect_stdout(io.StringIO())
        redirect.__enter__()
        self.addCleanup(redirect.__exit__, None, None, None)

    def recover(self):
        r.recover(p, f.C, self.root, self.op, AUDIT, TIME, self.reader, self.confirm)

    def test_recover_original_creation_then_resume_without_provider_post(self):
        original = p.local.private_read(self.obj.path)
        self.recover()
        obj = p.Provision(f.C, self.root, None)
        self.assertEqual(obj.ledger, dict(original, phase='complete', observed=self.reader.bucket))
        self.assertEqual(p.local.private_read(self.root / ('before-creation-recovery-' + self.op + '.json')), original)
        obj.completion(self.op, self.root / 'ready.json')
        self.assertEqual(self.confirm.call_count, 1)
        self.recover()
        self.assertEqual(self.confirm.call_count, 1)
        self.assertEqual([method for method, _, _ in self.cf.calls], ['GET'])
        with self.assertRaises(p.Error):
            obj.apply(self.op, True)

    def test_missing_wrong_failed_or_foreign_audit_rejects(self):
        original = copy.deepcopy(self.reader.audit)
        mutations = [dict(id='other'), dict(when='2026-10-14T20:38:03Z'), dict(owner=dict(id='b' * 32)),
                     dict(interface='dash'), dict(action=dict(type='delete', result=True)),
                     dict(action=dict(type='create', result=False)), dict(action=dict(type='create')),
                     dict(resource=dict(type='r2.bucket', id='foreign'))]
        for change in mutations:
            self.reader.audit = dict(original, **change)
            with self.assertRaises(r.RecoveryError):
                self.recover()
            self.assertEqual(p.local.private_read(self.obj.path)['phase'], 'applying')
        self.confirm.assert_not_called()

    def test_bucket_replaced_wrong_class_or_missing_generation_rejected(self):
        original = copy.deepcopy(self.reader.bucket)
        for change in [dict(name='foreign'), dict(creation_date='2026-10-14T20:38:20Z'),
                       dict(storage_class='InfrequentAccess'), dict(creation_date=None)]:
            self.reader.bucket = dict(original, **change)
            with self.assertRaises(r.RecoveryError):
                self.recover()
        self.confirm.assert_not_called()

    def test_no_blind_adoption_of_plan_or_changed_implementation(self):
        with patch.object(p, 'implementation_identity', return_value='changed'), self.assertRaises(p.Error):
            self.recover()
        self.obj.save('planned')
        with self.assertRaises(r.RecoveryError):
            self.recover()
        self.assertEqual(self.reader.calls, [])

    def test_fractional_timestamp_mismatch_reports_only_normalized_times(self):
        self.reader.audit['when'] = '2026-10-14T20:38:01.123456Z'
        with self.assertRaises(r.RecoveryError) as caught:
            self.recover()
        self.assertIn('API=2026-10-14T20:38:01.123456+00:00', str(caught.exception))
        self.assertIn('differs from supplied', str(caught.exception))
        self.assertEqual(p.local.private_read(self.obj.path)['phase'], 'applying')
        self.confirm.assert_not_called()
        self.reader.bucket['creation_date'] = self.reader.audit['when']
        r.recover(p, f.C, self.root, self.op, AUDIT, self.reader.audit['when'], self.reader, self.confirm)
        self.assertEqual(p.local.private_read(self.obj.path)['phase'], 'complete')

    def test_window_failure_is_distinct_and_cannot_be_bypassed_by_exact_time(self):
        self.reader.audit['when'] = '2026-10-14T21:38:01Z'
        with self.assertRaises(r.RecoveryError) as caught:
            r.recover(p, f.C, self.root, self.op, AUDIT, self.reader.audit['when'], self.reader, self.confirm)
        self.assertIn('outside original operation creation window', str(caught.exception))
        self.assertIn('original checkpoint=', str(caught.exception))
        self.assertEqual(p.local.private_read(self.obj.path)['phase'], 'applying')
        self.confirm.assert_not_called()

    def test_invalid_inventory_and_timestamp_fail_closed(self):
        for result in (None, [], [self.reader.audit, self.reader.audit], ['invalid']):
            with patch.object(self.reader, 'get', return_value=dict(result=result)), self.assertRaises(r.RecoveryError):
                self.recover()
        with self.assertRaises(r.RecoveryError):
            r.evidence(self.reader, f.C, AUDIT, TIME, '2026-10-14T20:28:00Z')
        with self.assertRaises(r.RecoveryError):
            r.instant('2026-10-14T20:38:01')

    def test_drift_during_confirmation_never_advances(self):
        def drift(*args):
            self.reader.bucket['creation_date'] = '2026-10-14T20:39:00Z'
        self.confirm.side_effect = drift
        with self.assertRaises(r.RecoveryError):
            self.recover()
        self.assertEqual(p.local.private_read(self.obj.path)['phase'], 'applying')

    def test_cancel_is_read_only(self):
        self.confirm.side_effect = RuntimeError('declined')
        with self.assertRaises(RuntimeError):
            self.recover()
        self.assertFalse((self.root / ('creation-recovery-' + self.op + '.json')).exists())
        self.assertEqual(p.local.private_read(self.obj.path)['phase'], 'applying')

    def test_crash_after_recovery_evidence_before_commit_resumes(self):
        real = p.local.save
        def fail(path, value):
            if path.name == 'operation.json':
                raise OSError('simulated crash')
            real(path, value)
        with patch.object(p.local, 'save', side_effect=fail), self.assertRaises(OSError):
            self.recover()
        self.recover()
        self.assertEqual(p.local.private_read(self.obj.path)['phase'], 'complete')

    def test_safe_status_codes_not_raw_provider_response_or_token(self):
        import io
        import urllib.error
        reader = r.Reader('SECRET_TOKEN', f.p.bootstrap.NoRedirect)
        error = urllib.error.HTTPError('https://private', 403, 'SECRET_MESSAGE', {},
                                      io.BytesIO(b'{"errors":[{"code":10000,"message":"SECRET_BODY"}]}'))
        with patch.object(reader.opener, 'open', side_effect=error):
            with self.assertRaises(r.RecoveryError) as caught:
                reader.get('/accounts/x/audit_logs', 'Audit verification')
        self.assertIn('HTTP 403', str(caught.exception))
        self.assertIn('10000', str(caught.exception))
        self.assertNotIn('SECRET', str(caught.exception))


if __name__ == '__main__':
    unittest.main()
