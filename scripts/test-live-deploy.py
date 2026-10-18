#!/usr/bin/env python3
"""Execute CI deployment shell against fake tools; never contact providers."""
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]


class DeployTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.bin = self.root / 'bin'
        self.bin.mkdir()
        self.artifact = self.root / 'candidate'
        for directory in ('infrastructure', 'relay', 'tools/node_modules/wrangler/bin'):
            (self.artifact / directory).mkdir(parents=True)
        (self.artifact / 'relay/IMAGE').write_text('registry.fly.io/bmux-live@sha256:' + 'd' * 64 + '\n')
        (self.artifact / 'relay/image.oci.tar').write_bytes(b'mock OCI')
        (self.artifact / 'REVISION').write_text('a' * 40 + '\n')
        (self.artifact / 'infrastructure/main.tf').write_text('# immutable test module\n')
        import hashlib
        files = sorted(p for p in self.artifact.rglob('*') if p.is_file())
        (self.artifact / 'SHA256SUMS').write_text(''.join(
            hashlib.sha256(p.read_bytes()).hexdigest() + '  ' + str(p.relative_to(self.artifact)) + '\n'
            for p in files))
        self.log = self.root / 'effects'
        self.env = dict(os.environ, PATH=str(self.bin) + os.pathsep + os.environ['PATH'],
                        TEST_EFFECTS=str(self.log), TEST_PLAN=json.dumps({'resource_changes': []}),
                        CLOUDFLARE_API_TOKEN='private-cf-token', CLOUDFLARE_ACCOUNT_ID='a' * 32,
                        CLOUDFLARE_ZONE_ID='b' * 32, LIVE_HOSTNAME='bmux.dev', LIVE_PAGES_PROJECT='bmux',
                        LIVE_METRICS_KEY_SHA256='e' * 64, FLY_API_TOKEN='private-fly-token',
                        LIVE_FLY_APP='bmux-live', LIVE_FLY_ORG='test-org',
                        LIVE_PUBLISHER_KEY_SHA256='c' * 64, R2_STATE_ACCESS_KEY_ID='private-access', R2_STATE_SECRET_ACCESS_KEY='private-secret', TOFU_STATE_ENCRYPTION_PASSPHRASE='private-secret',
                        LIVE_STATE_BUCKET='bmux-live-production', LIVE_STATE_REGION='auto',
                        AWS_ACCESS_KEY_ID='private-access', AWS_SECRET_ACCESS_KEY='private-secret',
                        AWS_SESSION_TOKEN='private-session',
                        GITHUB_REPOSITORY='owner/repo', GITHUB_SHA='a'*40,
                        GITHUB_RUN_ID='100', GITHUB_RUN_ATTEMPT='1',
                        GITHUB_OUTPUT=str(self.root / 'output'),
                        GITHUB_STEP_SUMMARY=str(self.root / 'summary'))
        for key in ('LIVE_STATE_ACCESS_KEY_ID', 'LIVE_STATE_SECRET_ACCESS_KEY', 'LIVE_REVIEWED_IMPORTS',
                    'LIVE_APPROVE_IMPORT'):
            self.env.pop(key, None)
        self.tool('tofu', '''import json,os,sys
from pathlib import Path
args=sys.argv[1:]
if args[0]=='version': print('{"terraform_version":"1.10.9"}')
elif args[0]=='state':
 with open(os.environ['TEST_EFFECTS'],'a') as f: f.write('tofu state\\n')
 if os.environ.get('TEST_FAIL')=='state': sys.exit(1)
 print(os.environ.get('TEST_MANAGED', ''))
elif args[0]=='show':
 sequence=json.loads(os.environ.get('TEST_PLAN_SEQUENCE','[]'))
 counter=Path(os.environ['TEST_EFFECTS']+'.shows')
 index=int(counter.read_text()) if counter.exists() else 0
 counter.write_text(str(index+1))
 print(json.dumps(sequence[min(index,len(sequence)-1)]) if sequence else os.environ['TEST_PLAN'])
else:
 with open(os.environ['TEST_EFFECTS'],'a') as f: f.write('tofu '+args[0]+'\\n')
 if args[0]=='plan':
  with open(os.environ['TEST_EFFECTS'],'a') as f: f.write('plan args '+ ' '.join(args[1:])+'\\n')
 if os.environ.get('TEST_FAIL')==args[0]:
  print('private-secret',file=sys.stderr); sys.exit(1)
''')
        self.tool('skopeo', "import os,sys\nsys.stdin.read() if sys.argv[1]=='login' else None\nwith open(os.environ['TEST_EFFECTS'],'a') as f: f.write('image '+sys.argv[1]+'\\n')\n")
        self.tool('node', "import os\nwith open(os.environ['TEST_EFFECTS'],'a') as f: f.write('pages upload\\n')\n")
        self.tool('python3', '''import json,os,sys
from pathlib import Path
args=sys.argv[1:]
if args and args[0].endswith('archive-live-state.py'):
 with open(os.environ['TEST_EFFECTS'],'a') as f: f.write('state '+args[1]+'\\n')
 if os.environ.get('TEST_FAIL')==args[1]: sys.exit(1)
 print(json.dumps({'sha256': os.environ.get('TEST_STATE_SHA256','f'*64)}))
elif args and args[0].endswith('live-fly-preflight.py'):
 with open(os.environ['TEST_EFFECTS'],'a') as f: f.write('fly ownership checked\\n')
elif args and args[0].endswith('configure-live-imports.py'):
 output=Path(args[args.index('--output')+1]); output.write_text('{"import": []}')
 reviewed=Path(args[args.index('--imports')+1]); assert json.loads(reviewed.read_text())=={}
 with open(os.environ['TEST_EFFECTS'],'a') as f: f.write('imports checked\\n')
elif args and args[0].endswith('smoke-live-broadcast.py'):
 assert args[args.index('--url')+1] == 'https://bmux.dev'
 assert args[args.index('--relay-url')+1] == 'https://live.bmux.dev'
 assert args[args.index('--component')+1] == 'backend'
 with open(os.environ['TEST_EFFECTS'],'a') as f: f.write('public smoke\\n')
else:
 os.execv(REAL_PYTHON,[REAL_PYTHON,*args])
'''.replace('REAL_PYTHON', repr(sys.executable)))

    def tool(self, name, source):
        path = self.bin / name
        path.write_text('#!' + sys.executable + '\n' + source)
        path.chmod(0o700)

    def run_deploy(self, mode='release', extra=()):
        result = subprocess.run([shutil.which('bash'), str(ROOT / 'scripts/live-deploy.sh'),
                                 'production', str(self.artifact), mode, *extra], env=self.env,
                                capture_output=True, text=True, timeout=15)
        self.assertNotIn('private-secret', result.stdout + result.stderr)
        return result, self.log.read_text() if self.log.exists() else ''

    def test_ambient_configuration_is_cleared_before_binding_state(self):
        source = (ROOT / 'scripts/live-deploy.sh').read_text()
        cleanup = source.split('# Clear ambient AWS/Tofu configuration', 1)[1]
        cleanup = cleanup.split('\n', 1)[1].split('\nexport AWS_ACCESS_KEY_ID=', 1)[0]
        probe = '''
for name in AWS_SESSION_TOKEN TF_CLI_ARGS TOFU_TEST_OVERRIDE; do
  [[ ! -v "$name" ]] || exit 1
done
[[ "$TOFU_STATE_ENCRYPTION_PASSPHRASE" == private-encryption-passphrase ]]
'''
        env = dict(self.env, TF_CLI_ARGS='-lock=false', TOFU_TEST_OVERRIDE='ambient',
                   TOFU_STATE_ENCRYPTION_PASSPHRASE='private-encryption-passphrase')
        result = subprocess.run([shutil.which('bash'), '-euc', cleanup + probe],
                                env=env, capture_output=True, text=True, timeout=5)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stderr, '')

    def test_release_uses_r2_and_smokes_backend_without_pages(self):
        result, effects = self.run_deploy()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn('pages upload', effects)
        self.assertLess(effects.index('tofu apply'), effects.index('public smoke'))
        self.assertLess(effects.index('image copy'), effects.index('tofu apply'))
        self.assertIn('public smoke', effects)
        self.assertNotIn('imports checked', effects)

    def test_preview_has_no_remote_writes_uploads_or_apply(self):
        result, effects = self.run_deploy('preview')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('state release-check', effects)
        self.assertIn('tofu plan', effects)
        for forbidden in ('state archive', 'image ', 'tofu apply', 'public smoke'):
            self.assertNotIn(forbidden, effects)

    def test_approved_infrastructure_matches_preview_before_any_production_write(self):
        self.env['TEST_PLAN'] = json.dumps({'resource_changes': [{
            'address': 'cloudflare_r2_bucket_lifecycle.state', 'change': {
                'actions': ['update'], 'before': {'rules': ['old']}, 'after': {'rules': ['new']}}}]})
        result, effects = self.run_deploy('preview')
        self.assertEqual(result.returncode, 0, result.stderr)
        for forbidden in ('state archive', 'image ', 'tofu apply', 'imports checked'):
            self.assertNotIn(forbidden, effects)
        self.env['LIVE_APPROVED_PLAN'] = (self.root / 'output').read_text().strip().split('=')[1]
        self.log.unlink()
        result, effects = self.run_deploy('approved')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertLess(effects.index('tofu plan'), effects.index('state archive'))
        self.assertLess(effects.index('state archive'), effects.index('image copy'))
        self.assertLess(effects.index('image copy'), effects.index('tofu apply'))
        self.assertNotIn('imports checked', effects)

    def test_missing_or_changed_approval_blocks_all_production_writes(self):
        result, _ = self.run_deploy('preview')
        self.assertEqual(result.returncode, 0, result.stderr)
        receipt = (self.root / 'output').read_text().strip().split('=')[1]
        for approved in ('', receipt):
            self.log.unlink(missing_ok=True)
            self.env['LIVE_APPROVED_PLAN'] = approved
            self.env['TEST_PLAN'] = json.dumps({'resource_changes': [{
                'address': 'cloudflare_dns_record.live', 'change': {'actions': ['update']}}]})
            result, effects = self.run_deploy('approved')
            self.assertNotEqual(result.returncode, 0)
            self.assertIn('new preview and approval', result.stderr)
            for forbidden in ('state archive', 'image ', 'tofu apply', 'imports checked'):
                self.assertNotIn(forbidden, effects)

    def test_incomplete_cutover_blocks_release_before_plan_and_archive(self):
        self.env['TEST_FAIL'] = 'release-check'
        result, effects = self.run_deploy()
        self.assertNotEqual(result.returncode, 0)
        for forbidden in ('tofu plan', 'state archive', 'image ', 'tofu apply'):
            self.assertNotIn(forbidden, effects)

    def test_removed_cutover_modes_fail_before_effects(self):
        for mode in ('migrate', 'preview-migration', 'recover', 'preview-recovery', 'retire-worker'):
            result, effects = self.run_deploy(mode)
            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(effects, '')

    def test_release_blocks_infrastructure_creation_before_apply(self):
        self.env['TEST_PLAN'] = json.dumps({'resource_changes': [
            {'address': 'cloudflare_pages_project.live', 'change': {'actions': ['create']}}]})
        result, effects = self.run_deploy()
        self.assertNotEqual(result.returncode, 0)
        self.assertNotIn('tofu apply', effects)
        self.assertNotIn('pages upload', effects)

    def test_configure_discovers_then_applies_and_cleans_imports(self):
        result, effects = self.run_deploy('configure')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('imports checked', effects)
        self.assertLess(effects.index('imports checked'), effects.index('tofu plan'))
        self.assertFalse((self.artifact / 'infrastructure/approved-imports.tf.json').exists())

    def test_apply_errors_use_structured_diagnostics(self):
        self.tool('tofu', '''import json,os,sys
args=sys.argv[1:]
if args[0]=='version': print('{"terraform_version":"1.10.9"}')
elif args[0]=='show': print(os.environ['TEST_PLAN'])
elif args[0]=='apply':
 assert '-json' in args
 print(json.dumps({'diagnostic': {'severity': 'error', 'summary': 'Fly app creation failed',
       'detail': 'HTTP 403 private-secret'}}))
 sys.exit(1)
''')
        result, effects = self.run_deploy()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('Deployment operation failed: tofu apply', result.stderr)
        self.assertIn('Fly app creation failed', result.stderr)
        self.assertIn('HTTP 403', result.stderr)
        self.assertNotIn('private-secret', result.stderr)

    def test_apply_failure_never_uploads_and_diagnostics_are_redacted(self):
        self.env['TEST_FAIL'] = 'apply'
        result, effects = self.run_deploy()
        self.assertNotEqual(result.returncode, 0)
        self.assertNotIn('pages upload', effects)

    def test_configure_creates_app_before_image_and_machine(self):
        self.env['TEST_MANAGED'] = 'cloudflare_worker.live'
        result, effects = self.run_deploy('configure')
        self.assertEqual(result.returncode, 0, result.stderr)
        plans = [line for line in effects.splitlines() if line.startswith('plan args')]
        self.assertEqual(len(plans), 2)
        self.assertIn('-target=fly_app.live', plans[0])
        self.assertNotIn('-target', plans[1])
        self.assertNotIn('live_migration_only', plans[1])
        self.assertEqual(effects.count('tofu apply'), 2)
        self.assertNotIn('pages upload', effects)
        self.assertLess(effects.rindex('state archive'), effects.index('public smoke'))
        self.assertNotIn('state begin', effects)
        self.log.unlink()
        self.env['TEST_FAIL'] = 'apply'
        result, effects = self.run_deploy('configure')
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(effects.count('tofu plan'), 1)
        self.assertNotIn('pages upload', effects)

    def test_existing_configure_rechecks_app_before_release(self):
        self.env['TEST_MANAGED'] = 'cloudflare_worker.live\ncloudflare_workers_deployment.live'
        result, effects = self.run_deploy('configure')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(effects.count('tofu plan'), 2)
        self.assertNotIn('live_migration_only', effects)

    def test_existing_state_list_errors_are_not_treated_as_first_use(self):
        self.env['TEST_FAIL'] = 'state'
        result, effects = self.run_deploy('configure')
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('tofu state', effects)
        self.assertNotIn('imports checked', effects)
        self.assertNotIn('tofu plan', effects)
        self.assertNotIn('pages upload', effects)

    def test_explicit_first_configure_fences_before_apply(self):
        receipt = self.root / 'completion.json'
        receipt.write_text('{}')
        self.env['TEST_FAIL'] = 'state'  # Real tofu state list fails for absent initial state.
        result, effects = self.run_deploy('configure', ('--bootstrap-completion', str(receipt)))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn('tofu state', effects)
        self.assertEqual(effects.count('state check'), 2)
        self.assertLess(effects.index('state admit'), effects.index('tofu init'))
        self.assertLess(effects.index('state begin'), effects.index('tofu apply'))
        self.assertLess(effects.index('tofu apply'), effects.rindex('state archive'))
        self.log.unlink()
        result, effects = self.run_deploy('release', ('--bootstrap-completion', str(receipt)))
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(effects, '')

    def test_ambiguous_auth_and_tampered_candidate_fail_before_provider(self):
        self.env.update(LIVE_STATE_ACCESS_KEY_ID='legacy', LIVE_STATE_SECRET_ACCESS_KEY='legacy-secret')
        result, effects = self.run_deploy()
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(effects, '')
        self.env.pop('LIVE_STATE_ACCESS_KEY_ID'); self.env.pop('LIVE_STATE_SECRET_ACCESS_KEY')
        (self.artifact / 'infrastructure/main.tf').write_text('tampered')
        result, effects = self.run_deploy()
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(effects, '')


if __name__ == '__main__':
    unittest.main()
