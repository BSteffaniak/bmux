#!/usr/bin/env python3
"""Offline mandatory volume, credential installer, and app collision checks."""
import contextlib
import importlib.util
import io
import json
from pathlib import Path
import os
import shutil
import sys
import tempfile
import subprocess
import unittest
from unittest.mock import patch
import urllib.error

ROOT = Path(__file__).resolve().parents[1]


def load(name):
    spec = importlib.util.spec_from_file_location(name, ROOT / 'scripts' / (name + '.py'))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class FlyTests(unittest.TestCase):
    def test_provider_receives_required_organization(self):
        source = (ROOT / 'infra/live-terraform/main.tf').read_text()
        self.assertRegex(source, r'provider "fly"\s*\{\s*org_slug\s*=\s*var\.fly_org\s*\}')
        deploy = (ROOT / 'scripts/live-deploy.sh').read_text()
        self.assertIn('TF_VAR_fly_org="$LIVE_FLY_ORG"', deploy)

    def test_acme_target_uses_cloudflare_canonical_dns_form(self):
        source = (ROOT / 'infra/live-terraform/fly.tf').read_text()
        expression = 'trimsuffix(fly_cert.live.acme_challenge_target, ".")'
        self.assertIn('content = ' + expression, source)
        if not shutil.which('tofu'):
            self.skipTest('OpenTofu unavailable')
        with tempfile.TemporaryDirectory() as temporary:
            for target in ('target.example.net.', 'target.example.net'):
                result = subprocess.run(['tofu', 'console'], cwd=temporary,
                    input='trimsuffix(' + json.dumps(target) + ', ".")\n',
                    text=True, capture_output=True, timeout=30, check=True)
                self.assertEqual(json.loads(result.stdout), 'target.example.net')

    def test_volume_reconciliation_is_paired_and_nondestructive(self):
        text = (ROOT / 'infra/live-terraform/fly.tf').read_text()
        # Both flags are required by the pinned provider: one updates volume
        # ownership and the other refreshes the machine's mounted reference.
        volume = text.split('resource "fly_volume" "authority" {', 1)[1].split('resource "fly_machine"', 1)[0]
        mount = text.split('  mount {', 1)[1].split('  }', 1)[0]
        self.assertIn('readopt_on_host_migration = true', volume)
        self.assertIn('readopt_on_host_migration = true', mount)
        self.assertIn('prevent_destroy = true', volume)
        self.assertIn('fly_volume.authority.id', mount)
        self.assertNotIn('ignore_changes', text)

    def test_required_private_volume(self):
        text = (ROOT / 'infra/live-terraform/fly.tf').read_text()
        for value in ['resource "fly_volume" "authority"', 'size_gb', 'encrypted',
                      'volume = fly_volume.authority.id', 'path   = "/data"',
                      'prevent_destroy = true', 'autostop      = "off"', 'memory_mb = 1024']:
            self.assertIn(' '.join(value.split()), ' '.join(text.split()))
        self.assertNotIn('local-exec', text)
        self.assertNotIn('create_before_destroy', text.replace('# Exactly one authority process; no autoscaler and no create-before-destroy.', ''))

    def test_credentials_pipe_only_no_state_bootstrap(self):
        module = load('setup-live-fly')
        calls = []
        variables = {'LIVE_FLY_APP': 'bmux-test', 'LIVE_FLY_ORG': 'example', 'LIVE_FLY_REGION': 'iad'}
        secrets = set()
        def run(args, **kwargs):
            calls.append((args, kwargs))
            if args[2] == 'set':
                secrets.add(args[3])
                return subprocess.CompletedProcess(args, 0, stdout=b'')
            rows = ([{'name': n, 'value': v} for n, v in variables.items()]
                    if args[1] == 'variable' else [{'name': n} for n in secrets])
            return subprocess.CompletedProcess(args, 0, stdout=json.dumps(rows).encode())
        discovery = {'organizations': {'nodes': [{'slug': 'example'}], 'pageInfo': {'hasNextPage': False}},
                     'platform': {'regions': [{'code': 'iad', 'deprecated': False}]}}
        output = io.StringIO()
        with patch.object(module, 'run_gh', side_effect=run), \
             patch.object(module, 'graphql', side_effect=[discovery, {'appNameAvailable': True}]), \
             patch.dict(module.os.environ, {}, clear=True), \
             patch('builtins.input', side_effect=['BACKED UP', 'I ACKNOWLEDGE BILLING']), \
             patch.object(module.getpass, 'getpass', side_effect=['private-fly-token', 'm' * 48, 'm' * 48]), \
             patch('sys.argv', ['setup-live-fly.py', '--approve']), contextlib.redirect_stdout(output):
            module.main()
        self.assertNotIn('private-fly-token', output.getvalue())
        self.assertNotIn('m' * 48, output.getvalue())
        writes = [(args, opts) for args, opts in calls if args[2] == 'set']
        self.assertEqual(len(writes), 2)
        self.assertEqual(writes[0][1]['input'], b'private-fly-token')
        for args, _ in calls:
            self.assertNotIn('private-fly-token', args)
            if args[1] == 'secret':
                self.assertIn('Production', args)

    def test_existing_credentials_unchanged(self):
        module = load('setup-live-fly')
        def inventory(args, **kwargs):
            rows = ([{'name': 'LIVE_FLY_APP', 'value': 'bmux-test'},
                     {'name': 'LIVE_FLY_ORG', 'value': 'example'},
                     {'name': 'LIVE_FLY_REGION', 'value': 'iad'}]
                    if args[1] == 'variable' else [
                        {'name': 'FLY_API_TOKEN'}, {'name': 'LIVE_METRICS_KEY_SHA256'}])
            return subprocess.CompletedProcess(args, 0, stdout=json.dumps(rows).encode())
        with patch.object(module, 'run_gh', side_effect=inventory) as run, \
             patch.object(module.getpass, 'getpass') as prompt, \
             patch.object(module, 'graphql') as network, \
             patch('sys.argv', ['setup-live-fly.py', '--approve']), contextlib.redirect_stdout(io.StringIO()):
            module.main()
        self.assertEqual(run.call_count, 2)
        prompt.assert_not_called()
        network.assert_not_called()

    def test_setup_wrapper_routes_fly_without_r2_bootstrap(self):
        with tempfile.TemporaryDirectory() as tmp:
            tool = Path(tmp) / 'python3'
            tool.write_text('#!' + sys.executable + '\nimport json,sys\nprint(json.dumps(sys.argv[1:]))\n')
            tool.chmod(0o700)
            env = dict(os.environ, BMUX_NIX_TOOLCHAIN='1', PATH=tmp + os.pathsep + os.environ['PATH'])
            result = subprocess.run([shutil.which('bash'), str(ROOT / 'scripts/setup-live.sh'),
                                     '--fly', '--repo', 'example/repo', '--approve'],
                                    env=env, capture_output=True, text=True, check=True)
            self.assertEqual(json.loads(result.stdout),
                             ['scripts/setup-live-fly.py', '--repo', 'example/repo', '--approve'])

    def test_workflow_uses_matching_production_secret_and_repository_variables(self):
        workflow = (ROOT / '.github/workflows/live-promote.yml').read_text()
        for value in ['environment: Production', 'secrets.FLY_API_TOKEN',
                      'secrets.LIVE_METRICS_KEY_SHA256', 'vars.LIVE_FLY_APP',
                      'vars.LIVE_FLY_ORG', 'vars.LIVE_FLY_REGION']:
            self.assertIn(value, workflow)

    def test_app_collision_and_missing_managed_fail_closed(self):
        module = load('live-fly-preflight')
        def missing(*args, **kwargs):
            raise urllib.error.HTTPError('private', 404, '', {}, None)
        module.inspect([], 'bmux-live', 'private', missing)
        with self.assertRaises(ValueError): module.inspect(['fly_app.live'], 'bmux-live', 'private', missing)
        def existing(*args, **kwargs): return contextlib.closing(io.BytesIO(b'{"name":"bmux-live"}'))
        with self.assertRaises(ValueError): module.inspect([], 'bmux-live', 'private', existing)
        module.inspect(['fly_app.live'], 'bmux-live', 'private', existing)


if __name__ == '__main__': unittest.main()
