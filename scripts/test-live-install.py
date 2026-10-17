#!/usr/bin/env python3
"""Offline installer tests: all provider and process tools mocked; never cloud calls."""
import contextlib
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('installer', Path(__file__).with_name('live-install.py'))
m = importlib.util.module_from_spec(spec)
spec.loader.exec_module(m)

CHOICES = dict(environment='staging', account_id='a' * 32, zone_id='b' * 32,
               hostname='live.example.com', pages_project='bmux-staging', publisher_key_sha256='c' * 64,
               bucket='bmux-test-state-staging', region='auto')
SECRET = 'PRIVATE_REUSABLE_TOKEN_DO_NOT_PRINT'


def candidate(root):
    root.mkdir()
    files = {'REVISION': 'd' * 40 + '\n', 'site/index.html': '<html>test fixture</html>',
             'relay/worker.js': '// offline fixture', 'tools/node_modules/wrangler/bin/wrangler.js': '// mock'}
    infra = Path(__file__).resolve().parent.parent / 'infra/live-terraform'
    for name in ('main.tf', 'encryption.tf', 'state.tf', '.terraform.lock.hcl'):
        files['infrastructure/' + name] = (infra / name).read_text()
    for name, value in files.items():
        path = root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(value)
    inventory(root)
    return root


def inventory(root):
    lines = [f'{m.digest(p)}  ./{p.relative_to(root)}' for p in sorted(root.rglob('*')) if p.is_file() and p.name != 'SHA256SUMS']
    (root / 'SHA256SUMS').write_text('\n'.join(lines) + '\n')


class MockTools:
    def __init__(self, artifact):
        self.artifact, self.env = artifact, {}
        self.node, self.tofu = '/mock/node', '/mock/tofu'
        self.calls, self.fail, self.diff = [], None, 0
        self.found, self.attached = {}, []

    def run(self, args, cwd=None, codes=(0,), timeout=900):
        self.calls.append(args)
        if 'pages' in args and self.fail == 'pages':
            raise m.InstallError('Mock interrupted Pages operation')
        data = {}
        if 'sts' in args:
            data = {'Account': '123456789012'}
        if 'list-buckets' in args:
            data = {'Buckets': []}
        if 'list-policies' in args:
            data = {'Policies': []}
        if 'list-attached-role-policies' in args:
            data = {'AttachedPolicies': self.attached}
        if 'attach-role-policy' in args:
            self.attached.append({'PolicyArn': args[-1]})
        return subprocess.CompletedProcess(args, 0, json.dumps(data).encode(), b'')

    def cf(self, path):
        return {'result': {'latest_deployment': {'deployment_trigger': {'metadata': {
            'commit_hash': (self.artifact / 'REVISION').read_text().strip(), 'branch': 'master'}}}}}

    def tofu_run(self, workspace, *args, codes=(0,)):
        self.calls.append(list(args))
        if args[0] == 'plan':
            for arg in args:
                if arg.startswith('-out='):
                    Path(arg[5:]).write_bytes(b'exact reviewed offline plan fixture')
            return subprocess.CompletedProcess(args, self.diff if '-detailed-exitcode' in args else 0, b'', b'')
        if args[0] == 'show':
            plan = {'resource_changes': [{'address': 'cloudflare_worker.live', 'change': {'actions': ['create']}}]}
            return subprocess.CompletedProcess(args, 0, json.dumps(plan).encode(), b'')
        if args[0] == 'apply' and self.fail == 'apply':
            raise m.InstallError('Mock apply timeout')
        if args[0] == 'state':
            return subprocess.CompletedProcess(args, 0, b'', b'')
        if args[0] == 'output':
            data = {'buckets': {'value': {'staging': CHOICES['bucket']}},
                    'state_policy_arns': {'value': {'staging': 'arn:aws:iam::123456789012:policy/bmux-live-state-staging'}}}
            return subprocess.CompletedProcess(args, 0, json.dumps(data).encode(), b'')
        return subprocess.CompletedProcess(args, 0, b'', b'')


class InstallerTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.base = Path(self.temp.name)
        self.root = self.base / 'private'
        m.private_dir(self.root)
        self.artifact = candidate(self.base / 'candidate')
        self.tools = MockTools(self.artifact)
        self.preflight = patch.object(m, 'preflight', return_value=CHOICES['account_id'])
        self.discovery = patch.object(m, 'discover', side_effect=lambda tools, c, managed=(): tools.found)
        self.preflight.start()
        self.discovery.start()
        self.addCleanup(self.preflight.stop)
        self.addCleanup(self.discovery.stop)
        self.addCleanup(self.temp.cleanup)

    def deploy(self, bootstrap=False):
        return m.Deployment(self.root, self.artifact, CHOICES.copy(), bootstrap, self.tools)

    def test_fly_candidate_rejects_legacy_local_apply(self):
        deployment = self.deploy()
        (self.artifact / 'infrastructure/fly.tf').write_text('# Fly candidate\n')
        with self.assertRaisesRegex(m.InstallError, 'approved configure/release workflow'):
            deployment.init()
        self.assertEqual(self.tools.calls, [])

    def reviewed(self, bootstrap=False):
        d = self.deploy(bootstrap)
        operation = d.plan({}, False, bootstrap)
        d.review()
        return d, operation

    def test_three_distinct_phases_and_idempotent_resume(self):
        d = self.deploy()
        operation = d.plan({}, False, False)
        self.assertFalse(any('apply' in args for args in self.tools.calls))
        with self.assertRaises(m.InstallError):
            d.apply(operation, False)
        review = d.review()
        self.assertEqual(operation, review['operation'])
        d.apply(operation, False)
        self.assertEqual(d.ledger['phase'], 'complete')
        count = len(self.tools.calls)
        self.deploy().apply(operation, False)
        self.assertEqual(count, len(self.tools.calls))

    def test_ambiguous_apply_never_replays(self):
        d, operation = self.reviewed()
        self.tools.fail = 'apply'
        with self.assertRaises(m.InstallError):
            d.apply(operation, False)
        restored = self.deploy()
        self.assertEqual(restored.ledger['phase'], 'applying')
        count = len(self.tools.calls)
        with self.assertRaises(m.InstallError):
            restored.apply(operation, False)
        self.assertEqual(count, len(self.tools.calls))
        self.tools.fail = None
        restored.reconcile(operation)
        restored.apply(operation, False)
        self.assertEqual(sum(args[0] == 'apply' for args in self.tools.calls), 1)

    def test_reconcile_diff_requires_explicit_abandon_new_identity(self):
        d, operation = self.reviewed()
        self.tools.fail = 'apply'
        with self.assertRaises(m.InstallError):
            d.apply(operation, False)
        self.tools.diff = 2
        with self.assertRaises(m.InstallError):
            d.reconcile(operation)
        d.abandon(operation)
        with self.assertRaises(m.InstallError):
            d.apply(operation, False)
        new = d.plan({}, False, False)
        self.assertNotEqual(new, operation)
        self.assertTrue((self.root / ('abandoned-' + operation + '.json')).exists())

    def test_pages_ambiguity_has_separate_ack_no_worker_reapply(self):
        d, operation = self.reviewed()
        d.phase('pages-uploading')  # Retained ledger from the old combined publisher.
        with self.assertRaises(m.InstallError):
            d.apply(operation, False)
        self.tools.fail = None
        d.ack_pages(operation)
        d.apply(operation, False)
        self.assertEqual(sum(args[0] == 'apply' for args in self.tools.calls), 0)

    def test_operation_and_content_identity(self):
        d, operation = self.reviewed()
        with self.assertRaises(m.InstallError):
            d.apply('0' * 32, False)
        (d.workspace / 'reviewed.plan').write_bytes(b'tampered')
        with self.assertRaises(m.InstallError):
            d.apply(operation, False)
        self.assertEqual(d.ledger['phase'], 'reviewed')

    def test_choice_and_artifact_resume_conflict(self):
        d, operation = self.reviewed()
        changed = CHOICES | {'hostname': 'other.example.com'}
        with self.assertRaises(m.InstallError):
            m.Deployment(self.root, self.artifact, changed, False, self.tools)
        (self.artifact / 'site/index.html').write_text('modified')
        with self.assertRaises(m.InstallError):
            d.apply(operation, False)

    def test_existing_resources_import_not_duplicate(self):
        self.tools.found = {'cloudflare_worker.live': 'account/existing'}
        d = self.deploy()
        with self.assertRaisesRegex(m.InstallError, 'Existing resources'):
            d.plan({}, False, False)
        self.assertFalse(any(args[0] == 'plan' for args in self.tools.calls))
        with self.assertRaises(m.InstallError):
            d.plan({'cloudflare_worker.live': 'account/existing'}, False, False)
        with self.assertRaises(m.InstallError):
            d.plan({'cloudflare_worker.live': 'account/wrong'}, True, False)
        d.plan({'cloudflare_worker.live': 'account/existing'}, True, False)
        data = m.private_read(d.workspace / 'approved-imports.tf.json')
        self.assertEqual(data['import'][0]['to'], '${cloudflare_worker.live}')

    def test_profile_secrets_rejected_and_no_secret_ledger(self):
        profile = {'schema': 2, 'choices': CHOICES}
        m.validate_profile(profile)
        with self.assertRaises(m.InstallError):
            m.validate_profile(profile | {'api_token': SECRET})
        self.tools.env['CLOUDFLARE_API_TOKEN'] = SECRET
        d, _ = self.reviewed()
        self.assertNotIn(SECRET, (self.root / 'ledger.json').read_text())
        self.assertNotIn(SECRET, json.dumps(d.review()))
        self.assertEqual((self.root / 'ledger.json').stat().st_mode & 0o777, 0o600)

    def test_backend_apply_never_uploads_pages(self):
        shutil.rmtree(self.artifact / 'site')
        inventory(self.artifact)
        m.verify_artifact(self.artifact)
        d, operation = self.reviewed()
        d.apply(operation, False)
        self.assertEqual(d.ledger['phase'], 'complete')
        self.assertNotIn('site/', '\n'.join(str(p.relative_to(self.artifact)) for p in self.artifact.rglob('*')))
        self.assertFalse(any('pages' in args for args in self.tools.calls))
        smoke = next(args for args in self.tools.calls if any('smoke-live-broadcast.py' in arg for arg in args))
        self.assertEqual(smoke[-2:], ['--component', 'backend'])

    def test_profile_and_ledger_corrupt_fail_closed(self):
        m.save(self.root / 'ledger.json', {'schema': 999})
        with self.assertRaises(m.InstallError):
            self.deploy()
        with self.assertRaises(m.InstallError):
            m.validate_profile({'schema': 1, 'choices': CHOICES})

    def test_private_files_and_symlinks_rejected(self):
        file = self.root / 'public.json'
        file.write_text('{}')
        file.chmod(0o644)
        with self.assertRaises(m.InstallError):
            m.private_read(file)
        link = self.root / 'link.json'
        link.symlink_to(file)
        with self.assertRaises(OSError):
            m.private_read(link)
        self.root.chmod(0o755)
        with self.assertRaises(m.InstallError):
            m.private_dir(self.root)

    def test_artifact_inventory_integrity_and_no_extra(self):
        m.verify_artifact(self.artifact)
        (self.artifact / 'extra.py').write_text('untrusted')
        with self.assertRaises(m.InstallError):
            m.verify_artifact(self.artifact)

    def test_symlink_or_traversal_manifest_fails(self):
        (self.artifact / 'link').symlink_to('/etc/passwd')
        with self.assertRaises(m.InstallError):
            m.verify_artifact(self.artifact)
        (self.artifact / 'SHA256SUMS').write_text('a' * 64 + '  ../outside\n')
        with self.assertRaises(m.InstallError):
            m.verify_artifact(self.artifact)

    def test_sanitized_env_disables_logs_and_cli_lock_overrides(self):
        env = {name: SECRET for name in m.SECRETS} | {'TF_LOG': 'TRACE', 'TF_CLI_ARGS_apply': '-lock=false',
                                                     'AWS_ENDPOINT_URL': 'http://evil', 'AWS_SESSION_TOKEN': SECRET}
        with patch.dict(os.environ, env, clear=True):
            result = m.environment(CHOICES)
        self.assertNotIn('TF_LOG', result)
        self.assertNotIn('TF_CLI_ARGS_apply', result)
        self.assertNotIn('AWS_ENDPOINT_URL', result)
        self.assertNotIn('AWS_SESSION_TOKEN', result)
        self.assertEqual(result['AWS_ACCESS_KEY_ID'], SECRET)

    def test_bootstrap_legacy_rejected(self):
        with self.assertRaises(m.InstallError):
            self.deploy(True)

    def test_bootstrap_completion_bound_and_fenced_before_apply(self):
        receipt = self.root / 'completion.json'
        receipt.write_text('{"offline": "receipt validated by archive subprocess"}')
        receipt.chmod(0o600)
        choices = dict(CHOICES, environment='production')
        deployment = m.Deployment(self.root, self.artifact, choices, False, self.tools, receipt)
        operation = deployment.plan({}, False, False)
        self.assertEqual(deployment.ledger['bootstrap_receipt_sha256'], m.digest(receipt))
        self.assertTrue(any('admit' in args for args in self.tools.calls))
        deployment.review()
        deployment.apply(operation, False)
        begin = next(i for i, args in enumerate(self.tools.calls) if 'begin' in args)
        apply = next(i for i, args in enumerate(self.tools.calls) if args[0] == 'apply')
        self.assertLess(begin, apply)
        self.assertEqual(deployment.ledger['phase'], 'complete')

    def test_r2_inventory_shapes_and_cursor_fail_closed(self):
        tools = m.Tools(self.artifact, {})
        row = {'name': CHOICES['bucket']}
        with patch.object(tools, 'cf', return_value={'result': {'buckets': [row]}}):
            self.assertEqual(tools.cf_list('accounts/id/r2/buckets'), [row])
        with patch.object(tools, 'cf', return_value={'result': {'rules': []}}):
            self.assertEqual(tools.cf_list('accounts/id/r2/buckets/name/lock'), [])
            self.assertEqual(tools.cf_list('accounts/id/r2/buckets/name/lifecycle'), [])
        for value in ({'result': {'buckets': [], 'cursor': 'more'}},
                      {'result': {'buckets': []}, 'result_info': {'cursor': 'more'}}):
            with patch.object(tools, 'cf', return_value=value):
                with self.assertRaises(m.InstallError):
                    tools.cf_list('accounts/id/r2/buckets')

    def test_provider_account_changed_rejects_apply(self):
        d, operation = self.reviewed()
        with patch.object(m, 'preflight', return_value='999999999999'):
            with self.assertRaises(m.InstallError):
                d.apply(operation, False)
        self.assertEqual(d.ledger['phase'], 'reviewed')

    def test_plan_destructive_replacement_blocked(self):
        with self.assertRaises(m.InstallError):
            m.review_summary({'resource_changes': [{'address': 'cloudflare_worker.live', 'change': {'actions': ['delete', 'create']}}]})

    def test_import_validation(self):
        self.assertEqual(m.imports(['cloudflare_worker.live=account/worker']), {'cloudflare_worker.live': 'account/worker'})
        for value in ['unknown.foo=id', 'cloudflare_worker.live=secret\nvalue', 'cloudflare_worker.live=']:
            with self.assertRaises(m.InstallError):
                m.imports([value])

    def test_process_diagnostics_secret_safe(self):
        tools = m.Tools(self.artifact, {})
        result = subprocess.CompletedProcess(['mock'], 1, SECRET.encode(), SECRET.encode())
        with patch.object(m.subprocess, 'run', return_value=result):
            with self.assertRaises(m.InstallError) as error:
                tools.run(['mock'])
        self.assertNotIn(SECRET, str(error.exception))
        with patch.object(m.subprocess, 'run', side_effect=subprocess.TimeoutExpired(['mock'], 1, SECRET)):
            with self.assertRaises(m.InstallError) as error:
                tools.run(['mock'])
        self.assertNotIn(SECRET, str(error.exception))

    def test_doctor_offline_no_credentials_and_guide_missing_profile(self):
        with patch.object(m.Tools, 'check'), patch.object(m.Tools, 'cf', side_effect=AssertionError('cloud call')):
            with self.assertRaisesRegex(m.InstallError, '--setup'):
                m.main(['--doctor', '--artifact', str(self.artifact), '--state-dir', str(self.root)])

    def test_release_urls_reject_credentials_and_mutable_digest_missing(self):
        m.validate_release('https://example.com/release.tar.gz', 'a' * 64)
        for url, checksum in [('http://example.com/a', 'a' * 64), ('https://u:p@example.com/a', 'a' * 64),
                              ('https://example.com/a?token=secret', 'a' * 64), ('https://example.com/a', '')]:
            with self.assertRaises(m.InstallError):
                m.validate_release(url, checksum)

    def test_download_approved_hash_then_inventory(self):
        archive = self.base / 'archive.tar.gz'
        with tarfile.open(archive, 'w:gz') as tar:
            for path in self.artifact.rglob('*'):
                tar.add(path, arcname=str(path.relative_to(self.artifact)), recursive=False)
        raw = archive.read_bytes()
        response = io.BytesIO(raw)
        opener = unittest.mock.Mock()
        opener.open.return_value = response
        expected = hashlib.sha256(raw).hexdigest()
        with patch.object(m.urllib.request, 'build_opener', return_value=opener):
            actual = m.acquire_candidate('https://example.com/approved.tar.gz', expected, self.root / 'downloads')
        self.assertEqual(m.verify_artifact(actual), m.verify_artifact(self.artifact))

    def test_download_wrong_digest_rejected(self):
        opener = unittest.mock.Mock()
        opener.open.return_value = io.BytesIO(b'not approved package')
        with patch.object(m.urllib.request, 'build_opener', return_value=opener):
            with self.assertRaises(m.InstallError):
                m.acquire_candidate('https://example.com/a', 'a' * 64, self.root / 'downloads')

    def interactive_main(self, responses):
        root = self.root / 'default'
        m.private_dir(root)
        m.save(root / 'profile.json', {'schema': 2, 'choices': CHOICES})
        self.addCleanup(lambda: None)
        def factory(artifact, env):
            self.tools.env = env
            return self.tools
        with patch.object(m.sys.stdin, 'isatty', return_value=True), patch.object(m, 'Tools', side_effect=factory), \
                patch.object(m, 'environment', return_value={}), patch('builtins.input', side_effect=responses), \
                contextlib.redirect_stdout(io.StringIO()):
            return m.main(['--state-dir', str(self.root), '--artifact', str(self.artifact)])

    def test_interactive_single_command_plans_reviews_approves_applies(self):
        def answer(prompt):
            if 'Need new AWS' in prompt:
                return ''
            if 'read-only locked plan' in prompt:
                return 'PLAN'
            if 'type operation ' in prompt:
                return prompt.split('type operation ')[1].split(':')[0]
            raise AssertionError(prompt)
        self.assertEqual(self.interactive_main(answer), 0)
        self.assertEqual(m.private_read(self.root / 'default/ledger.json')['phase'], 'complete')
        self.assertEqual(sum(args[0] == 'apply' for args in self.tools.calls), 1)

    def test_interactive_cancel_after_review_retains_plan_no_apply(self):
        self.assertEqual(self.interactive_main(['PLAN', 'cancel']), 0)
        self.assertEqual(m.private_read(self.root / 'default/ledger.json')['phase'], 'reviewed')
        self.assertFalse(any(args[0] == 'apply' for args in self.tools.calls))

    def test_interactive_cancel_before_plan_no_provider_calls(self):
        self.assertEqual(self.interactive_main(['cancel']), 0)
        self.assertEqual(self.tools.calls, [])

    def test_explicit_legacy_bootstrap_rejected_before_provider(self):
        with self.assertRaises(m.InstallError):
            m.main(['--bootstrap', '--non-interactive'])
        self.assertEqual(self.tools.calls, [])

    def test_interactive_setup_release_and_cancel_without_secret_persistence(self):
        root = self.root / 'fresh-profile'
        m.private_dir(root)
        output = io.StringIO()
        with patch.object(m, 'select_provider_defaults', return_value={}), \
                patch.dict(os.environ, {}, clear=True), \
                patch('builtins.input', side_effect=list(CHOICES.values())), contextlib.redirect_stdout(output):
            m.setup(root / 'profile.json')
        with patch('builtins.input', side_effect=['https://example.com/approved.tar.gz', 'a' * 64]):
            m.setup_release(root / 'release.json', interactive=True)
        self.assertEqual(m.validate_profile(m.private_read(root / 'profile.json')), CHOICES)
        self.assertEqual(m.private_read(root / 'release.json')['sha256'], 'a' * 64)
        self.assertNotIn(SECRET, (root / 'profile.json').read_text())

    def test_corrupt_summary_cannot_leak_credentials(self):
        d, _ = self.reviewed()
        ledger = dict(d.ledger)
        ledger['summary'] = [{'address': 'cloudflare_worker.live', 'actions': ['create'], 'import': False, 'token': SECRET}]
        m.save(d.ledger_path, ledger)
        with self.assertRaises(m.InstallError):
            self.deploy()

    def test_private_plan_copy_changed_hash_blocks_reapply(self):
        d, operation = self.reviewed()
        (d.workspace / 'reviewed.plan').write_text('unexpected content')
        with self.assertRaises(m.InstallError):
            self.deploy().apply(operation, False)
        self.assertFalse(any(args[0] == 'apply' for args in self.tools.calls))

    def test_interactive_import_approval_then_exact_apply(self):
        self.tools.found = {'cloudflare_worker.live': 'account/existing'}
        def answer(prompt):
            if 'Need new AWS' in prompt:
                return ''
            if 'read-only locked plan' in prompt:
                return 'PLAN'
            if 'imports' in prompt:
                return 'IMPORT'
            if 'type operation ' in prompt:
                return prompt.split('type operation ')[1].split(':')[0]
            raise AssertionError(prompt)
        self.assertEqual(self.interactive_main(answer), 0)
        ledger = m.private_read(self.root / 'default/ledger.json')
        self.assertEqual(ledger['imports'], self.tools.found)
        self.assertEqual(ledger['phase'], 'complete')

    def test_discovery_uses_prompt_token_without_env_or_profile_persistence(self):
        tools = unittest.mock.Mock()
        tools.admission_args = []
        tools.cf_list.side_effect = [[{'id': 'a' * 32, 'name': SECRET}],
                                    [{'id': 'b' * 32, 'name': 'example.com', 'status': 'active'}]]
        output = io.StringIO()
        with patch.object(m, 'Tools', return_value=tools), patch.dict(os.environ, {}, clear=True), \
                patch.object(m.getpass, 'getpass', return_value=SECRET), \
                patch('builtins.input', side_effect=['DISCOVER', '1', '1']), contextlib.redirect_stdout(output):
            choices = m.select_provider_defaults()
        self.assertEqual(choices['hostname'], 'live.example.com')
        self.assertNotIn(SECRET, json.dumps(choices))
        self.assertNotIn(SECRET, output.getvalue())

    def test_local_cli_help_and_extracted_installer_doctor(self):
        bundle = self.root / 'extracted'
        m.private_dir(bundle)
        for name in ('live-install.py', 'live-state.py', 'smoke-live-broadcast.py'):
            shutil.copyfile(Path(__file__).with_name(name), bundle / name)
        result = subprocess.run([os.sys.executable, '-I', str(bundle / 'live-install.py'), '--help'],
                                stdin=subprocess.DEVNULL, capture_output=True, check=False)
        self.assertEqual(result.returncode, 0)
        self.assertIn(b'--reconcile', result.stdout)
        result = subprocess.run([os.sys.executable, '-I', str(bundle / 'live-install.py'), '--doctor',
                                 '--state-dir', str(self.root / 'doctor-state'), '--artifact', str(self.base / 'missing')],
                                stdin=subprocess.DEVNULL, capture_output=True, check=False)
        self.assertEqual(result.returncode, 2)
        self.assertIn(b'Candidate unavailable', result.stderr)
        self.assertNotIn(b'Traceback', result.stderr)

    def test_plan_review_only_exposes_addresses_actions_not_secret_values(self):
        plan = {'resource_changes': [{'address': 'cloudflare_worker.live',
                                     'change': {'actions': ['update'], 'before': {'token': SECRET},
                                                'after': {'publisher': SECRET}},
                                     'provider_config': {'token': SECRET}}],
                'configuration': {'provider_config': SECRET}}
        summary = m.review_summary(plan)
        self.assertNotIn(SECRET, json.dumps(summary))
        self.assertEqual(summary[0]['actions'], ['update'])

    def test_download_tar_traversal_rejected(self):
        archive = self.base / 'traversal.tar.gz'
        with tarfile.open(archive, 'w:gz') as tar:
            member = tarfile.TarInfo('../outside')
            member.size = 1
            tar.addfile(member, io.BytesIO(b'x'))
        raw = archive.read_bytes()
        opener = unittest.mock.Mock()
        opener.open.return_value = io.BytesIO(raw)
        with patch.object(m.urllib.request, 'build_opener', return_value=opener):
            with self.assertRaises(m.InstallError):
                m.acquire_candidate('https://example.com/a', hashlib.sha256(raw).hexdigest(), self.root / 'downloads')
        self.assertFalse((self.root / 'outside').exists())

    def test_download_cache_manifest_cannot_redefine_archive_identity(self):
        archive = self.base / 'cache.tar.gz'
        with tarfile.open(archive, 'w:gz') as tar:
            for path in self.artifact.rglob('*'):
                tar.add(path, arcname=str(path.relative_to(self.artifact)), recursive=False)
        raw = archive.read_bytes()
        expected = hashlib.sha256(raw).hexdigest()
        opener = unittest.mock.Mock()
        opener.open.return_value = io.BytesIO(raw)
        with patch.object(m.urllib.request, 'build_opener', return_value=opener):
            actual = m.acquire_candidate('https://example.com/a', expected, self.root / 'downloads')
        (actual / 'site/index.html').write_text('modified')
        inventory(actual)
        with self.assertRaises(m.InstallError):
            m.acquire_candidate('https://example.com/a', expected, self.root / 'downloads')

    def test_real_discovery_uses_shared_ownership_policy(self):
        self.discovery.stop()
        tools = unittest.mock.Mock()
        tools.admission_args = []
        bucket = {'name': CHOICES['bucket'], 'storage_class': 'Standard', 'creation_date': '2026-01-01'}
        responses = {
            f'accounts/{CHOICES["account_id"]}/pages/projects': [],
            f'accounts/{CHOICES["account_id"]}/r2/buckets': [bucket],
            f'accounts/{CHOICES["account_id"]}/r2/buckets/{CHOICES["bucket"]}': [bucket],
            f'accounts/{CHOICES["account_id"]}/r2/buckets/{CHOICES["bucket"]}/lock': [],
            f'accounts/{CHOICES["account_id"]}/r2/buckets/{CHOICES["bucket"]}/lifecycle': [],
            f'zones/{CHOICES["zone_id"]}/dns_records?name=live.example.com': [],
            f'accounts/{CHOICES["account_id"]}/workers/workers': [],
            f'accounts/{CHOICES["account_id"]}/workers/scripts': [],
            f'zones/{CHOICES["zone_id"]}/workers/routes': [],
        }
        tools.cf_list.side_effect = responses.__getitem__
        result = m.discover(tools, CHOICES)
        self.assertEqual(result, {'cloudflare_r2_bucket.state': CHOICES['account_id'] + '/' + CHOICES['bucket'] + '/default'})

    def test_real_preflight_rejects_wrong_zone_account_and_expired_token(self):
        self.preflight.stop()
        tools = unittest.mock.Mock()
        tools.admission_args = []
        tools.env = {}
        tools.run.return_value = subprocess.CompletedProcess([], 0, b'{"Account":"123456789012"}', b'')
        tools.cf.side_effect = [{'result': {'status': 'expired'}}]
        with self.assertRaises(m.InstallError):
            m.preflight(tools, CHOICES)
        tools.cf.side_effect = [{'result': {'status': 'active'}}, {'result': {}},
                                {'result': {'account': {'id': '0' * 32}, 'status': 'active', 'name': 'example.com'}}]
        with self.assertRaises(m.InstallError):
            m.preflight(tools, CHOICES)
        self.assertEqual(tools.run.call_count, 0)  # no state calls after rejected CF auth

    def test_provider_inventory_unknown_shape_never_appears_empty(self):
        tools = m.Tools(self.artifact, {})
        with patch.object(tools, 'cf', return_value={'result': {'unexpected': []}}):
            with self.assertRaises(m.InstallError):
                tools.cf_list('accounts/x/workers/workers')

    def test_real_preflight_locked_backend_security(self):
        self.preflight.stop()
        tools = unittest.mock.Mock()
        tools.admission_args = []
        tools.cf.side_effect = [{'result': {'status': 'active'}}, {'result': {}},
                                {'result': {'status': 'active', 'account': {'id': CHOICES['account_id']}, 'name': 'example.com'}}]
        self.assertEqual(m.preflight(tools, CHOICES), CHOICES['account_id'])
        self.assertEqual(tools.run.call_count, 1)
        self.assertIn('archive-live-state.py', tools.run.call_args.args[0][-2])
        self.assertEqual(tools.run.call_args.args[0][-1], 'check')

    def test_completed_operation_archived_before_new_release_plan(self):
        d, operation = self.reviewed()
        d.apply(operation, False)
        (self.artifact / 'REVISION').write_text('e' * 40 + '\n')
        inventory(self.artifact)
        new = self.deploy()
        with self.assertRaises(m.InstallError):
            new.apply(operation, False)
        next_operation = new.plan({}, False, False)
        self.assertNotEqual(operation, next_operation)
        self.assertTrue((self.root / ('completed-' + operation + '.json')).exists())

    def test_pages_ack_requires_authoritative_revision(self):
        d, operation = self.reviewed()
        d.phase('pages-uploading')  # Legacy ambiguous upload still needs reconciliation.
        with patch.object(self.tools, 'cf', return_value={'result': {'latest_deployment': {'deployment_trigger': {'metadata': {'commit_hash': '0' * 40, 'branch': 'master'}}}}}):
            with self.assertRaises(m.InstallError):
                d.ack_pages(operation)
        self.assertEqual(d.ledger['phase'], 'pages-uploading')

    def test_default_candidate_precedence_explicit_artifact_and_saved_release(self):
        root = self.root / 'default'
        m.private_dir(root)
        m.save(root / 'profile.json', {'schema': 2, 'choices': CHOICES})
        m.save(root / 'release.json', {'schema': 1, 'url': 'https://example.com/release.tar.gz', 'sha256': 'a' * 64})
        common = ['--state-dir', str(self.root), '--default-artifact', str(self.base / 'absent-default'), '--non-interactive']
        with patch.object(m, 'acquire_candidate', return_value=self.artifact) as acquire, contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(m.main(common), 0)
            acquire.assert_called_once()
        with patch.object(m, 'acquire_candidate', side_effect=AssertionError('explicit artifact must win')), contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(m.main(common + ['--artifact', str(self.artifact)]), 0)
        with patch.object(m, 'acquire_candidate', side_effect=AssertionError('missing explicit artifact must fail closed')):
            with self.assertRaises(m.InstallError):
                m.main(common + ['--artifact', str(self.base / 'absent-explicit')])
        with self.assertRaises(m.InstallError):
            m.main(common + ['--artifact', str(self.artifact), '--candidate-url', 'https://example.com/a', '--candidate-sha256', 'b' * 64])
        with self.assertRaises(m.InstallError):
            m.main(['--artifact', 'relative-path', '--non-interactive'])

    def test_doctor_reports_selected_exact_missing_candidate_with_release_fix(self):
        selected = self.base / 'selected-slot/bin/live-candidate'
        with self.assertRaises(m.InstallError) as caught:
            m.main(['--doctor', '--state-dir', str(self.root), '--default-artifact', str(selected)])
        message = str(caught.exception)
        self.assertIn(str(selected), message)
        self.assertIn('packaged-release HTTPS URL', message)
        self.assertIn('independently trusted archive SHA-256', message)
        self.assertIn('No repository build', message)

    def test_no_cache_relative_candidate_fallback(self):
        root = self.root / 'default'
        m.private_dir(root)
        m.save(root / 'profile.json', {'schema': 2, 'choices': CHOICES})
        with self.assertRaisesRegex(m.InstallError, 'No installed candidate'):
            m.main(['--state-dir', str(self.root), '--non-interactive'])

    def test_download_archive_symlinks_rejected(self):
        archive = self.base / 'unsafe.tar.gz'
        with tarfile.open(archive, 'w:gz') as tar:
            member = tarfile.TarInfo('unsafe-link')
            member.type = tarfile.SYMTYPE
            member.linkname = '/etc/passwd'
            tar.addfile(member)
        raw = archive.read_bytes()
        opener = unittest.mock.Mock()
        opener.open.return_value = io.BytesIO(raw)
        with patch.object(m.urllib.request, 'build_opener', return_value=opener):
            with self.assertRaises(m.InstallError):
                m.acquire_candidate('https://example.com/a', hashlib.sha256(raw).hexdigest(), self.root / 'downloads')


if __name__ == '__main__':
    unittest.main()
