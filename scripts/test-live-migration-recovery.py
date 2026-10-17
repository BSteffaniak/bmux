#!/usr/bin/env python3
"""Recovery policy, approval binding, shell ordering, and conditional completion."""
import copy
import importlib.util
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]


def load(name):
    spec = importlib.util.spec_from_file_location(name.replace('-', '_'), ROOT / 'scripts' / (name + '.py'))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


M = load('live-migration-recovery')
FIX = load('test-live-migration-plan')
DEPLOY = load('test-live-deploy')
FENCE = load('test-live-migration-fence')


def plan():
    value = FIX.plan()
    value['resource_changes'] = [r for r in value['resource_changes'] if not r['address'].startswith('cloudflare_worker')]
    value['resource_changes'][0] = FIX.resource('fly_app.live', ['no-op'], {'id': 'app'}, {'id': 'app'})
    return value


def evidence():
    return {'state': {'sha256': 'f' * 64}, 'marker': {'phase': 'applying'}, 'etag': 'etag'}


class RecoveryTests(unittest.TestCase):
    def test_plan_diagnostics_exclude_private_values(self):
        candidate = plan()
        candidate['resource_changes'].append(FIX.resource('PRIVATE_ADDRESS', ['PRIVATE_ACTION'],
                                                         {'secret': 'PRIVATE_VALUE'}))
        report = M.plan_diagnostics(candidate)
        text = json.dumps(report)
        self.assertNotIn('PRIVATE', text)
        self.assertIn('fly_app.live', text)
        self.assertEqual(report['changes'][-1]['actions'], ['unknown'])
        changed = next(r for r in candidate['resource_changes'] if r['address'] == 'fly_app.live')
        changed['change']['actions'] = ['update']
        report = M.plan_diagnostics(candidate)
        self.assertEqual(report['changes'][0]['actions'], ['update'])
        with self.assertRaises(ValueError):
            M.validate(candidate)

    def test_policy_preserves_app_and_rejects_destructive_and_unrelated_changes(self):
        M.validate(plan())
        for resource in (FIX.resource('fly_app.live', ['create']),
                         FIX.resource('fly_machine.live', ['update']),
                         FIX.resource('fly_volume.authority', ['delete']),
                         FIX.resource('cloudflare_worker.live', ['delete']),
                         FIX.resource('cloudflare_pages_domain.live', ['update'])):
            candidate = plan()
            candidate['resource_changes'] = [r for r in candidate['resource_changes'] if r['address'] != resource['address']]
            candidate['resource_changes'].append(resource)
            with self.subTest(resource=resource['address']), self.assertRaises(ValueError):
                M.validate(candidate)

    def test_completed_resource_plan_can_retry_health_without_recreation(self):
        candidate = plan()
        candidate['applyable'] = False
        for resource in candidate['resource_changes']:
            resource['change']['actions'] = ['no-op']
            resource['change']['before'] = resource['change']['after']
        M.validate(candidate)

    def test_digest_binds_marker_state_image_and_candidate(self):
        with tempfile.TemporaryDirectory() as tmp:
            artifact = Path(tmp)
            (artifact / 'REVISION').write_text('a' * 40)
            (artifact / 'SHA256SUMS').write_text('inventory')
            original = M.digest(plan(), evidence(), artifact, {})
            altered = evidence()
            altered['etag'] = 'changed'
            self.assertNotEqual(original, M.digest(plan(), altered, artifact, {}))
            altered = evidence()
            altered['state']['sha256'] = 'e' * 64
            self.assertNotEqual(original, M.digest(plan(), altered, artifact, {}))
            candidate = plan()
            candidate['resource_changes'][-1]['change']['after']['image'] = 'different'
            self.assertNotEqual(original, M.digest(candidate, evidence(), artifact, {}))
            self.assertNotEqual(original, M.digest(plan(), evidence(), artifact, {'LIVE_FLY_ORG': 'other'}))

    def test_completion_uses_original_conditional_marker_identity(self):
        with patch.dict(os.environ, FENCE.ENV):
            store = FENCE.FakeR2()
            self.addCleanup(store.temp.cleanup)
            receipt = store.migration_begin(FENCE.DIGEST, FENCE.SOURCE)
            snapshot = M.snapshot(store)
            self.assertEqual(snapshot['receipt'], receipt)
            receipt_path = store.directory / 'receipt.json'
            FENCE.m.save_private(receipt_path, snapshot['receipt'])
            result = store.migration_complete(receipt_path)
            self.assertEqual(result['status'], 'completed')
            self.assertEqual(store.migration_marker()[0]['phase'], 'completed')
            with self.assertRaises(ValueError):
                M.snapshot(store)

    def test_workflow_never_builds_and_site_requires_recovery_success(self):
        workflow = (ROOT / '.github/workflows/live-recover.yml').read_text()
        task = (ROOT / '.github/workflows/live-recovery-task.yml').read_text()
        self.assertIn('environment: production-approval', workflow)
        self.assertIn('needs: [preview, approve]', workflow)
        self.assertIn('needs: [recover]', workflow)
        self.assertIn('reuse_candidate: true', workflow)
        self.assertIn('needs.preview.outputs.digest', workflow)
        for text in (workflow, task):
            self.assertNotIn('cargo ', text)
            self.assertNotIn('docker build', text)
        self.assertIn('APPLY: ${{ inputs.apply }}', task)
        self.assertLess(task.index('Reuse verified site before recovery'),
                        task.index('Preview or apply explicit partial-cutover recovery'))
        self.assertIn('group: bmux-production-infrastructure', task)


class RecoveryShellTests(DEPLOY.DeployTests):
    def configure_recovery(self):
        self.env['TEST_PLAN'] = json.dumps(plan())
        self.env['TEST_MANAGED'] = 'fly_app.live'
        self.env['LIVE_RECOVERY_DIGEST'] = ''
        wrapper = self.bin / 'python3'
        source = wrapper.read_text()
        source = source.replace("if args and args[0].endswith('archive-live-state.py'):", '''if args and args[0].endswith('live-migration-recovery.py') and args[1]=='verify':
 with open(os.environ['TEST_EFFECTS'],'a') as f: f.write('recovery verified\\n')
elif args and args[0].endswith('live-migration-recovery.py') and args[1]=='snapshot':
 Path(args[args.index('--evidence')+1]).write_text(json.dumps({'state': {'sha256': 'f'*64}, 'marker': {'phase': 'applying'}, 'etag': 'etag'}))
 with open(os.environ['TEST_EFFECTS'],'a') as f: f.write('recovery snapshot\\n')
elif args and args[0].endswith('live-migration-recovery.py') and args[1]=='complete':
 with open(os.environ['TEST_EFFECTS'],'a') as f: f.write('recovery complete\\n')
elif args and args[0].endswith('archive-live-state.py'):''')
        source = source.replace("elif args and args[0].endswith('smoke-live-broadcast.py'):",
                                "elif args and args[0].endswith('smoke-live-broadcast.py') and os.environ.get('TEST_FAIL_HEALTH'):\n sys.exit(1)\nelif args and args[0].endswith('smoke-live-broadcast.py'):")
        wrapper.write_text(source)

    def approval(self):
        self.configure_recovery()
        result, _ = self.run_deploy('preview-recovery')
        self.assertEqual(result.returncode, 0, result.stderr)
        return result.stdout.split('Recovery approval digest: ')[1].splitlines()[0]

    def test_recovery_uses_existing_app_then_health_before_completion(self):
        self.env['LIVE_RECOVERY_DIGEST'] = self.approval()
        result, effects = self.run_deploy('recover')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertLess(effects.index('recovery verified'), effects.index('tofu apply'))
        self.assertLess(effects.index('image copy'), effects.index('tofu apply'))
        self.assertLess(effects.index('tofu apply'), effects.index('public smoke'))
        self.assertLess(effects.index('public smoke'), effects.index('recovery complete'))
        for forbidden in ('-destroy', '-target=fly_app.live', 'migration-begin', 'migration-check'):
            self.assertNotIn(forbidden, effects)

    def test_stale_approval_never_applies(self):
        self.configure_recovery()
        self.env['LIVE_RECOVERY_DIGEST'] = '0' * 64
        result, effects = self.run_deploy('recover')
        self.assertNotEqual(result.returncode, 0)
        self.assertNotIn('tofu apply', effects)
        self.assertNotIn('image copy', effects)

    def test_failed_health_never_completes(self):
        self.env['LIVE_RECOVERY_DIGEST'] = self.approval()
        self.env['TEST_FAIL_HEALTH'] = '1'
        result, effects = self.run_deploy('recover')
        self.assertNotEqual(result.returncode, 0)
        self.assertNotIn('recovery complete', effects)

    def test_failed_apply_never_completes(self):
        self.env['LIVE_RECOVERY_DIGEST'] = self.approval()
        self.env['TEST_FAIL'] = 'apply'
        result, effects = self.run_deploy('recover')
        self.assertNotEqual(result.returncode, 0)
        self.assertNotIn('recovery complete', effects)


if __name__ == '__main__':
    unittest.main()
