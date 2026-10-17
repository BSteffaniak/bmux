#!/usr/bin/env python3
"""Hermetic approval binding and phase-fencing tests; no provider access."""
import contextlib
import copy
import importlib.util
import io
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location('migration_plan', ROOT / 'scripts/live-migration-plan.py')
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


def resource(address, actions, before=None, after=None, unknown=None):
    return dict(address=address, change=dict(actions=actions, before=before, after=after, after_unknown=unknown or {}))


def plan():
    return {'resource_changes': [
        resource('cloudflare_worker.live', ['delete'], {'id': 'old-worker', 'private': 'NEVER_PRINT'}),
        resource('fly_app.live', ['create'], after={'name': 'bmux-live', 'id': None}, unknown={'id': True}),
        resource('fly_volume.authority', ['create'], after={'name': 'authority', 'id': None}, unknown={'id': True}),
        resource('fly_machine.live', ['create'], after={'image': 'registry.fly.io/bmux-live@sha256:' + 'a'*64,
                                                      'app': None}, unknown={'app': True})]}


class MigrationPlanTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.artifact = Path(self.temp.name)
        (self.artifact / 'REVISION').write_text('b' * 40)
        (self.artifact / 'SHA256SUMS').write_text('candidate-inventory')
        self.state = {'sha256': 'c' * 64}

    def test_approval_bound_to_state_candidate_config_and_private_plan(self):
        original = MODULE.digest(plan(), self.state, self.artifact, {})
        self.assertEqual(original, MODULE.digest(plan(), self.state, self.artifact, {}))
        changed = plan()
        changed['resource_changes'][0]['change']['before']['id'] = 'different-worker'
        for value in [MODULE.digest(changed, self.state, self.artifact, {}),
                      MODULE.digest(plan(), {'sha256': 'd' * 64}, self.artifact, {}),
                      MODULE.digest(plan(), self.state, self.artifact, {'LIVE_FLY_ORG': 'other'})]:
            self.assertNotEqual(original, value)
        (self.artifact / 'SHA256SUMS').write_text('changed-inventory')
        self.assertNotEqual(original, MODULE.digest(plan(), self.state, self.artifact, {}))

    def test_reject_other_deletions_and_replacements_imports_unknown_addresses(self):
        for item in [resource('fly_volume.authority', ['delete']), resource('fly_machine.live', ['delete', 'create']),
                     resource('cloudflare_dns_record.unrelated', ['update']),
                     resource('cloudflare_worker.other', ['delete']),
                     resource('cloudflare_r2_bucket.state', ['update']),
                     resource('cloudflare_pages_project.live', ['create'])]:
            value = plan()
            value['resource_changes'].append(item)
            with self.assertRaises(ValueError): MODULE.changes(value)
        for key, value in [('importing', {'id': 'adopt'}), ('actions', ['create'])]:
            candidate = plan()
            candidate['resource_changes'][0]['change'][key] = value
            with self.assertRaises(ValueError): MODULE.changes(candidate)
        with self.assertRaises(ValueError): MODULE.digest({'resource_changes': []}, self.state, self.artifact, {})

    def test_phase_may_resolve_unknowns_but_not_change_approved_config(self):
        approved = plan()
        phase = copy.deepcopy(approved)
        phase['resource_changes'] = phase['resource_changes'][2:]
        phase['resource_changes'][-1]['change']['after']['app'] = 'resolved-app'
        MODULE.validate_phase(approved, phase, 'resources')
        phase['resource_changes'][-1]['change']['after']['image'] = 'unexpected-image'
        with self.assertRaises(ValueError): MODULE.validate_phase(approved, phase, 'resources')
        phase = plan()
        phase['resource_changes'][0]['change']['before']['id'] = 'new-worker'
        with self.assertRaises(ValueError): MODULE.validate_phase(approved, phase, 'resources')
        phase = {'resource_changes': [resource('fly_ip.v4', ['create'])]}
        with self.assertRaises(ValueError): MODULE.validate_phase(approved, phase, 'resources')

    def test_missing_and_noop_phase_mutations_are_rejected(self):
        for stage in ('retire', 'app', 'resources'):
            with self.assertRaises(ValueError):
                MODULE.validate_phase(plan(), {'resource_changes': []}, stage)
        candidate = {'resource_changes': [resource('cloudflare_worker.live', ['no-op'], {'id': 'old-worker'}, {'id': 'old-worker'})]}
        with self.assertRaises(ValueError): MODULE.validate_phase(plan(), candidate, 'retire')

    def test_sparse_nested_unknowns_and_completed_resources(self):
        self.assertTrue(MODULE.compatible({}, {'nested': {'id': 'resolved'}}, {'nested': {'id': True}}))
        self.assertFalse(MODULE.compatible({}, {'nested': {'id': 'resolved', 'unexpected': 'value'}}, {'nested': {'id': True}}))
        approved = plan()
        phase = {'resource_changes': approved['resource_changes'][2:] + [resource('fly_app.live', ['no-op'],
                    after={'name': 'bmux-live', 'id': 'resolved'})]}
        MODULE.validate_phase(approved, phase, 'resources')
        phase['resource_changes'][-1]['change']['after']['name'] = 'different-app'
        with self.assertRaises(ValueError): MODULE.validate_phase(approved, phase, 'resources')

    def test_only_safe_summary_and_digest_leave_private_inputs(self):
        source = self.artifact / 'plan.json'
        source.write_text(json.dumps(plan()))
        state = self.artifact / 'state.json'
        state.write_text(json.dumps(self.state))
        summary = self.artifact / 'summary'
        output = self.artifact / 'output'
        with patch.dict(os.environ, {'GITHUB_STEP_SUMMARY': str(summary), 'GITHUB_OUTPUT': str(output)}), \
             patch('sys.argv', ['live-migration-plan.py', str(source), '--state', str(state), '--artifact', str(self.artifact)]), \
             contextlib.redirect_stdout(io.StringIO()) as stdout:
            MODULE.main()
        self.assertNotIn('NEVER_PRINT', stdout.getvalue() + summary.read_text() + output.read_text())
        self.assertIn('cloudflare_worker.live', summary.read_text())
        self.assertRegex(output.read_text(), r'^migration_digest=[a-f0-9]{64}\n$')
        with patch('sys.argv', ['live-migration-plan.py', str(source), '--state', str(state), '--artifact', str(self.artifact), '--expected', '0'*64]), \
             contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
            MODULE.main()


if __name__ == '__main__':
    unittest.main()
