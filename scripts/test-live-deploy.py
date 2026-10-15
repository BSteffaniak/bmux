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
        for directory in ('infrastructure', 'relay', 'site', 'tools/node_modules/wrangler/bin'):
            (self.artifact / directory).mkdir(parents=True)
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
                        LIVE_PUBLISHER_KEY_SHA256='c' * 64, R2_STATE_ACCESS_KEY_ID='private-access', R2_STATE_SECRET_ACCESS_KEY='private-secret', TOFU_STATE_ENCRYPTION_PASSPHRASE='private-secret',
                        LIVE_STATE_BUCKET='bmux-live-production', LIVE_STATE_REGION='auto',
                        AWS_ACCESS_KEY_ID='private-access', AWS_SECRET_ACCESS_KEY='private-secret',
                        AWS_SESSION_TOKEN='private-session')
        for key in ('LIVE_STATE_ACCESS_KEY_ID', 'LIVE_STATE_SECRET_ACCESS_KEY', 'LIVE_REVIEWED_IMPORTS',
                    'LIVE_APPROVE_IMPORT'):
            self.env.pop(key, None)
        self.tool('tofu', '''import json,os,sys
from pathlib import Path
args=sys.argv[1:]
if args[0]=='version': print('{"terraform_version":"1.10.9"}')
elif args[0]=='show': print(os.environ['TEST_PLAN'])
else:
 with open(os.environ['TEST_EFFECTS'],'a') as f: f.write('tofu '+args[0]+'\\n')
 if os.environ.get('TEST_FAIL')==args[0]:
  print('private-secret',file=sys.stderr); sys.exit(1)
''')
        self.tool('node', "import os\nwith open(os.environ['TEST_EFFECTS'],'a') as f: f.write('pages upload\\n')\n")
        self.tool('python3', '''import json,os,sys
from pathlib import Path
args=sys.argv[1:]
if args and args[0].endswith('archive-live-state.py'):
 with open(os.environ['TEST_EFFECTS'],'a') as f: f.write('state '+args[1]+'\\n')
elif args and args[0].endswith('configure-live-imports.py'):
 output=Path(args[args.index('--output')+1]); output.write_text('{"import": []}')
 reviewed=Path(args[args.index('--imports')+1]); assert json.loads(reviewed.read_text())=={}
 with open(os.environ['TEST_EFFECTS'],'a') as f: f.write('imports checked\\n')
elif args and args[0].endswith('smoke-live-broadcast.py'):
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

    def test_release_uses_r2_and_uploads_after_apply(self):
        result, effects = self.run_deploy()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertLess(effects.index('tofu apply'), effects.index('pages upload'))
        self.assertIn('public smoke', effects)
        self.assertNotIn('imports checked', effects)

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

    def test_apply_failure_never_uploads_and_diagnostics_are_redacted(self):
        self.env['TEST_FAIL'] = 'apply'
        result, effects = self.run_deploy()
        self.assertNotEqual(result.returncode, 0)
        self.assertNotIn('pages upload', effects)

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
