#!/usr/bin/env python3
"""Offline behavioral coverage of incident-scoped state repair."""
import copy
import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

SPEC = importlib.util.spec_from_file_location('reconcile', Path(__file__).with_name('reconcile-live-mounts.py'))
M = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(M)


def state():
    return {'version': 4, 'terraform_version': '1.10.9', 'serial': 12, 'lineage': 'incident-test',
            'outputs': {'secret': {'value': 'private-test-only', 'type': 'string', 'sensitive': True}},
            'resources': [
                {'mode': 'managed', 'type': 'fly_machine', 'name': 'live', 'provider': M.PROVIDER,
                 'instances': [{'schema_version': 1, 'attributes': {
                     'id': M.MACHINE, 'app': M.APP, 'region': 'iad', 'env': {'SECRET': 'private-env'},
                     'mount': [{'path': '/data', 'volume': M.PREVIOUS, 'readopt_on_host_migration': False}]}}]},
                {'mode': 'managed', 'type': 'fly_volume', 'name': 'authority', 'provider': M.PROVIDER,
                 'instances': [{'schema_version': 0, 'attributes': {
                     'id': M.PREVIOUS, 'app': M.APP, 'name': 'live_authority', 'region': 'iad',
                     'encrypted': True, 'size_gb': 1, 'attached_machine_id': '',
                     'readopt_on_host_migration': False}}]},
                {'mode': 'managed', 'type': 'unrelated', 'name': 'keep', 'provider': M.PROVIDER,
                 'instances': [{'schema_version': 0, 'private': 'cHJlc2VydmU=', 'attributes': {'id': 'preserve'}}]}]}


def get(path, token):
    if path.endswith('/machines/' + M.MACHINE):
        return {'id': M.MACHINE, 'region': 'iad', 'config': {'mounts': [{'path': '/data', 'volume': M.LIVE}]}}
    if path == 'apps/' + M.APP + '/volumes':
        return [{'id': M.LIVE, 'attached_machine_id': M.MACHINE, 'name': 'live_authority',
                 'region': 'iad', 'encrypted': True, 'size_gb': 1}]
    raise AssertionError('Unexpected API path')


class ReconciliationTests(unittest.TestCase):
    def test_preserves_all_state_except_exact_ownership_and_flags(self):
        original = state()
        result = M.prepare(original, M.APP, get)
        expected = state()
        expected['serial'] += 1
        volume = expected['resources'][1]['instances'][0]['attributes']
        volume.update(id=M.LIVE, attached_machine_id=M.MACHINE, readopt_on_host_migration=True)
        mount = expected['resources'][0]['instances'][0]['attributes']['mount'][0]
        mount.update(volume=M.LIVE, readopt_on_host_migration=True)
        self.assertEqual(result, expected)
        self.assertEqual(original, state())
        again = M.prepare(result, M.APP, get)
        again['serial'] = result['serial']
        self.assertEqual(again, result)

    def test_unknown_identity_schema_and_authority_fail_closed(self):
        mutations = [lambda s: s.update(version=5), lambda s: s.update(serial='12'),
                     lambda s: s.update(terraform_version='1.11.0'),
                     lambda s: s['resources'][0]['instances'][0].update(schema_version=99),
                     lambda s: s['resources'][0].update(provider='other-provider'),
                     lambda s: s['resources'][0]['instances'][0]['attributes'].update(id='unexpected'),
                     lambda s: s['resources'][1]['instances'][0]['attributes'].update(id='vol_unreviewed'),
                     lambda s: s['resources'].append(copy.deepcopy(s['resources'][0]))]
        for mutate in mutations:
            original = state()
            mutate(original)
            with self.assertRaises(M.ReconciliationError):
                M.prepare(original, M.APP, get)
        with self.assertRaises(M.ReconciliationError):
            M.prepare(state(), 'another-app', get)

    def test_live_drift_ambiguity_and_old_volume_reappearance_block_repair(self):
        for change in ('old_exists', 'detached', 'unencrypted', 'wrong_region', 'wrong_mount'):
            def drift(path, token):
                value = get(path, token)
                if isinstance(value, list):
                    if change == 'old_exists': value.append({'id': M.PREVIOUS})
                    if change == 'detached': value[0]['attached_machine_id'] = None
                    if change == 'unencrypted': value[0]['encrypted'] = False
                    if change == 'wrong_region': value[0]['region'] = 'ord'
                elif change == 'wrong_mount':
                    value['config']['mounts'][0]['volume'] = M.PREVIOUS
                return value
            with self.subTest(change=change), self.assertRaises(M.ReconciliationError):
                M.prepare(state(), M.APP, drift)

    @unittest.skipUnless(shutil.which('tofu'), 'OpenTofu unavailable')
    def test_real_encrypted_push_preserves_freshness_and_lineage_guards(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / 'main.tf').write_text(Path(__file__).resolve().parents[1].joinpath(
                'infra/live-terraform/encryption.tf').read_text())
            env = dict(os.environ, TF_VAR_state_encryption_passphrase='isolated-test-only-passphrase-123456789')
            for key in list(env):
                if key.startswith(('TF_CLI_ARGS', 'TF_DATA_DIR', 'TOFU_')):
                    env.pop(key)
            def run(*args, success=True):
                result = subprocess.run(['tofu', '-chdir=' + directory, *args], env=env,
                                        capture_output=True, text=True, timeout=30)
                if success: self.assertEqual(result.returncode, 0, result.stderr)
                else: self.assertNotEqual(result.returncode, 0)
                return result
            run('init', '-input=false')
            run('apply', '-input=false', '-auto-approve')
            # Seed only this isolated encrypted backend with the incident-shaped
            # fixture, then execute the actual candidate transformation.
            seed = state()
            seed['lineage'] = json.loads(run('state', 'pull').stdout)['lineage']
            (root / 'seed.json').write_text(json.dumps(seed))
            run('state', 'push', '-lock-timeout=5m', str(root / 'seed.json'))
            pulled = json.loads(run('state', 'pull').stdout)
            candidate = M.prepare(pulled, M.APP, get)
            # Simulate a cooperating writer between pull and push. An equal
            # serial with different content must reject this candidate too.
            concurrent = copy.deepcopy(pulled)
            concurrent['serial'] += 1
            concurrent['outputs']['other'] = {'value': 'concurrent', 'type': 'string'}
            (root / 'concurrent.json').write_text(json.dumps(concurrent))
            run('state', 'push', '-lock-timeout=5m', str(root / 'concurrent.json'))
            (root / 'candidate.json').write_text(json.dumps(candidate))
            run('state', 'push', '-lock-timeout=5m', str(root / 'candidate.json'), success=False)
            # Re-read instead of silently rebasing the rejected candidate.
            pulled = json.loads(run('state', 'pull').stdout)
            candidate = M.prepare(pulled, M.APP, get)
            (root / 'candidate.json').write_text(json.dumps(candidate))
            run('state', 'push', '-lock-timeout=5m', str(root / 'candidate.json'))
            canonical = json.loads(run('state', 'pull').stdout)
            normalized = copy.deepcopy(canonical)
            normalized['serial'] = candidate['serial']
            self.assertEqual(normalized, candidate)
            self.assertEqual(canonical['outputs'], candidate['outputs'])
            self.assertGreater(canonical['serial'], pulled['serial'])
            encrypted = json.loads((root / 'terraform.tfstate').read_text())
            self.assertEqual(encrypted['encryption_version'], 'v0')
            self.assertNotIn('outputs', encrypted)
            run('state', 'push', '-lock-timeout=5m', str(root / 'candidate.json'), success=False)
            candidate['lineage'] = 'different-authority'
            candidate['serial'] = canonical['serial']
            (root / 'candidate.json').write_text(json.dumps(candidate))
            run('state', 'push', '-lock-timeout=5m', str(root / 'candidate.json'), success=False)
            self.assertEqual(json.loads(run('state', 'pull').stdout), canonical)


if __name__ == '__main__':
    unittest.main()
