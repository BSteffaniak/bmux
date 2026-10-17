#!/usr/bin/env python3
"""Exercise migration shell phases against hermetic provider/state doubles."""
import importlib.util
import json
from pathlib import Path
import unittest

ROOT = Path(__file__).resolve().parents[1]

def load(name):
    spec = importlib.util.spec_from_file_location(name.replace('-', '_'), ROOT / 'scripts' / (name + '.py'))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module

DEPLOY = load('test-live-deploy')
PLAN = load('test-live-migration-plan')


class MigrationDeployTests(DEPLOY.DeployTests):
    def configure_migration(self):
        approved = PLAN.plan()
        self.env['TEST_PLAN'] = json.dumps(approved)
        self.env['TEST_MANAGED'] = 'cloudflare_worker.live\ncloudflare_workers_route.live'
        self.env['LIVE_RETIRE_WORKER'] = 'retire-bmux-live-worker'
        self.env['LIVE_ENVIRONMENT'] = 'production'
        self.env['LIVE_MIGRATION_DIGEST'] = PLAN.MODULE.digest(approved, {'sha256': 'f'*64}, self.artifact, self.env)
        retire = {'resource_changes': [approved['resource_changes'][0]]}
        app = {'resource_changes': [approved['resource_changes'][1]]}
        remaining = {'resource_changes': approved['resource_changes'][2:]}
        self.env['TEST_PLAN_SEQUENCE'] = json.dumps([approved, retire, retire, app, remaining])
        return approved

    def test_plan_failure_reports_operation_and_redacted_provider_diagnostic(self):
        self.configure_migration()
        self.tool('tofu', '''import json,os,sys
args=sys.argv[1:]
if args[0]=='version':
 print('{"terraform_version":"1.10.9"}')
elif args[0]=='state':
 print(os.environ.get('TEST_MANAGED',''))
elif args[0]=='plan':
 print(json.dumps({'diagnostic': {'severity': 'error', 'summary': 'Provider rejected request',
       'detail': 'HTTP 403 private-secret private-cf-token'}}))
 sys.exit(1)
''')
        result, effects = self.run_deploy('check-migration')
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('Deployment operation failed: tofu plan', result.stderr)
        self.assertIn('Provider rejected request', result.stderr)
        self.assertIn('HTTP 403', result.stderr)
        self.assertNotIn('private-cf-token', result.stderr)
        self.assertNotIn('state migration-begin', effects)

    def test_check_wrapper_uses_only_infrastructure_and_never_applies(self):
        self.configure_migration()
        output = self.root / 'infrastructure-check'
        result = DEPLOY.subprocess.run(
            [DEPLOY.shutil.which('bash'), str(ROOT / 'scripts/check-live-infrastructure.sh'), str(output)],
            env=self.env, capture_output=True, text=True, timeout=15)
        self.assertEqual(result.returncode, 0, result.stderr)
        effects = self.log.read_text()
        self.assertIn('tofu plan', effects)
        self.assertNotIn('tofu apply', effects)
        self.assertNotIn('state migration-begin', effects)
        self.assertNotIn('Approval digest:', result.stdout)
        self.assertFalse((output / 'relay/image.oci.tar').exists())

    def test_fast_infrastructure_check_never_applies_or_emits_approval(self):
        self.configure_migration()
        result, effects = self.run_deploy('check-migration')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('Authenticated infrastructure plan succeeded', result.stdout)
        self.assertNotIn('Approval digest:', result.stdout)
        for forbidden in ('tofu apply', 'image ', 'state archive', 'state migration-begin'):
            self.assertNotIn(forbidden, effects)

    def test_preview_no_mutations_or_archives_or_registry_upload(self):
        self.configure_migration()
        result, effects = self.run_deploy('preview-migration')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('Approval digest:', result.stdout)
        for effect in ('state archive', 'state migration-begin', 'tofu apply', 'image ', 'pages upload'):
            self.assertNotIn(effect, effects)
        self.assertIn('state migration-check', effects)

    def test_single_approved_cutover_fenced_and_archived_in_order(self):
        self.configure_migration()
        result, effects = self.run_deploy('migrate')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(effects.count('tofu apply'), 3)
        self.assertEqual(effects.count('image copy'), 1)
        self.assertNotIn('pages upload', effects)
        self.assertLess(effects.index('state archive'), effects.index('state migration-begin'))
        self.assertLess(effects.index('state migration-begin'), effects.index('tofu apply'))
        self.assertLess(effects.index('public smoke'), effects.index('state migration-complete'))
        self.assertEqual(effects.count('state migration-begin'), 1)

    def test_stale_approval_before_any_mutation(self):
        self.configure_migration()
        self.env['LIVE_MIGRATION_DIGEST'] = '0'*64
        result, effects = self.run_deploy('migrate')
        self.assertNotEqual(result.returncode, 0)
        self.assertNotIn('tofu apply', effects)
        self.assertNotIn('state archive', effects)
        self.assertNotIn('image ', effects)

    def test_fence_failure_prevents_all_resource_effects(self):
        self.configure_migration()
        self.env['TEST_FAIL'] = 'migration-begin'
        result, effects = self.run_deploy('migrate')
        self.assertNotEqual(result.returncode, 0)
        self.assertNotIn('tofu apply', effects)
        self.assertNotIn('image ', effects)
        self.assertNotIn('migration-complete', effects)

    def test_partial_apply_stops_no_automatic_second_phase(self):
        self.configure_migration()
        self.env['TEST_FAIL'] = 'apply'
        result, effects = self.run_deploy('migrate')
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(effects.count('tofu apply'), 1)
        self.assertIn('state migration-begin', effects)
        self.assertNotIn('image ', effects)
        self.assertNotIn('migration-complete', effects)

    def test_changed_phase_cannot_widen_approved_mutations(self):
        approved = self.configure_migration()
        changed = PLAN.resource('cloudflare_workers_route.live', ['delete'], {'id': 'different-route'})
        self.env['TEST_PLAN_SEQUENCE'] = json.dumps([approved, {'resource_changes': [changed]}])
        result, effects = self.run_deploy('migrate')
        self.assertNotEqual(result.returncode, 0)
        self.assertNotIn('tofu apply', effects)
        self.assertNotIn('migration-complete', effects)

    def test_existing_migration_fence_blocks_preview(self):
        self.configure_migration()
        self.env['TEST_FAIL'] = 'migration-check'
        result, effects = self.run_deploy('preview-migration')
        self.assertNotEqual(result.returncode, 0)
        self.assertNotIn('tofu plan', effects)

    def test_migration_cannot_bootstrap_or_adopt(self):
        self.configure_migration()
        for name, value in [('LIVE_BOOTSTRAP_COMPLETION', 'unexpected'), ('LIVE_REVIEWED_IMPORTS', '{"fly_app.live":"other"}'), ('LIVE_APPROVE_IMPORT','true')]:
            self.env[name] = value
            result, effects = self.run_deploy('migrate')
            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(effects, '')
            del self.env[name]


if __name__ == '__main__':
    unittest.main()
