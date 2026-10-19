#!/usr/bin/env python3
"""Offline guided Fly setup tests; all network, gh and interaction are mocked."""
import contextlib
import hashlib
import importlib.util
import io
import json
from pathlib import Path
import subprocess
import sys
import unittest
from unittest.mock import patch
import urllib.error

ROOT = Path(__file__).resolve().parents[1]


def load():
    spec = importlib.util.spec_from_file_location('guided_fly', ROOT / 'scripts/setup-live-fly.py')
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


CONFIG = {'LIVE_FLY_APP': 'bmux-test', 'LIVE_FLY_ORG': 'example', 'LIVE_FLY_REGION': 'iad'}
DISCOVERY = {'organizations': {'nodes': [{'slug': 'example'}], 'pageInfo': {'hasNextPage': False}},
             'platform': {'regions': [{'code': 'iad', 'deprecated': False}]}}


class GuidedTests(unittest.TestCase):
    def run_setup(self, *, variables=None, secrets=(), approve=True, answers=None,
                  private=None, discovery=None, available=True, fail_set=None, env=None):
        module = load()
        variables = dict(variables or {})
        secret_names = set(secrets)
        calls, writes = [], []
        def run(args, **kwargs):
            calls.append((args, kwargs))
            self.assertEqual(kwargs['timeout'], 30)
            self.assertNotIn('FLY_API_TOKEN', kwargs['env'])
            kind, action = args[1:3]
            if action == 'list':
                rows = ([{'name': n, 'value': v} for n, v in variables.items()]
                        if kind == 'variable' else [{'name': n} for n in secret_names])
                return subprocess.CompletedProcess(args, 0, stdout=json.dumps(rows).encode())
            self.assertEqual(action, 'set')
            name = args[3]
            if name == fail_set:
                raise subprocess.CalledProcessError(1, args, stderr=b'private-token')
            writes.append((kind, name, kwargs['input']))
            if kind == 'variable':
                self.assertNotIn(name, variables)
                variables[name] = kwargs['input'].decode()
            else:
                self.assertNotIn(name, secret_names)
                secret_names.add(name)
            return subprocess.CompletedProcess(args, 0, stdout=b'')
        def network(token, query, variables=None):
            self.assertEqual(token, 'private-token')
            if query == module.DISCOVERY:
                if isinstance(discovery, BaseException):
                    raise discovery
                return DISCOVERY if discovery is None else discovery
            self.assertEqual(query, module.AVAILABILITY)
            self.assertEqual(variables, {'name': 'bmux-test'})
            return {'appNameAvailable': available}
        out, err = io.StringIO(), io.StringIO()
        with patch.object(module, 'run_gh', side_effect=run), \
             patch.object(module, 'graphql', side_effect=network) as api, \
             patch.object(module.getpass, 'getpass', side_effect=private or ['private-token', 'm' * 48, 'm' * 48]) as prompt, \
             patch('builtins.input', side_effect=answers if answers is not None else ['BACKED UP', 'I ACKNOWLEDGE BILLING']) as public, \
             patch.dict(module.os.environ, env or {}, clear=True), \
             patch('sys.argv', ['setup-live-fly.py'] + (['--approve'] if approve else [])), \
             contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            try:
                module.main()
                status = 0
            except SystemExit as error:
                status = error.code
        for text in [out.getvalue(), err.getvalue(), repr([x[0] for x in calls])]:
            self.assertNotIn('private-token', text)
            self.assertNotIn('m' * 48, text)
        return status, writes, calls, api, prompt, public, out.getvalue(), err.getvalue()

    def test_credentials_pipe_only(self):
        status, writes, *_ = self.run_setup(variables=CONFIG,
            answers=['BACKED UP', 'I ACKNOWLEDGE BILLING'])
        self.assertEqual(status, 0)
        self.assertEqual(len(writes), 2)
        self.assertTrue(all(kind == 'secret' for kind, _, _ in writes))
        self.assertIn(('secret', 'FLY_API_TOKEN', b'private-token'), writes)
        self.assertIn(('secret', 'LIVE_METRICS_KEY_SHA256', hashlib.sha256(b'm' * 48).hexdigest().encode()), writes)

    def test_missing_public_config_requires_terraform_without_writes(self):
        status, writes, _, api, prompt, *_ = self.run_setup()
        self.assertEqual(status, 2)
        self.assertEqual(writes, [])
        api.assert_not_called()
        prompt.assert_not_called()

    def test_repeat_preserves_credentials_without_provider_calls(self):
        status, writes, _, api, prompt, *_ = self.run_setup(variables=CONFIG,
            secrets=['FLY_API_TOKEN', 'LIVE_METRICS_KEY_SHA256'])
        self.assertEqual(status, 0)
        self.assertEqual(writes, [])
        api.assert_not_called()
        prompt.assert_not_called()

    def test_no_approval_means_no_secret_writes(self):
        status, writes, _, api, prompt, *_ = self.run_setup(variables=CONFIG, approve=False)
        self.assertEqual(status, 0)
        self.assertEqual(writes, [])
        api.assert_not_called()
        prompt.assert_not_called()

    def test_partial_failure_can_resume_without_replacing_token(self):
        status, writes, *_ = self.run_setup(variables=CONFIG,
            answers=['BACKED UP', 'I ACKNOWLEDGE BILLING'], fail_set='LIVE_METRICS_KEY_SHA256')
        self.assertEqual(status, 2)
        status, writes, *_ = self.run_setup(variables=CONFIG, secrets=['FLY_API_TOKEN'],
            answers=['BACKED UP', 'I ACKNOWLEDGE BILLING'])
        self.assertEqual(status, 0)
        self.assertEqual(len(writes), 1)
        self.assertEqual(writes[0][1], 'LIVE_METRICS_KEY_SHA256')

    def test_cancellation_and_invalid_metrics_make_no_writes(self):
        for private, answers in [(['private-token', 'short', 'short'], []),
                (['private-token', 'm' * 48, 'n' * 48], []),
                (['private-token', 'm' * 48, 'm' * 48], ['no'])]:
            status, writes, *_ = self.run_setup(variables=CONFIG, private=private, answers=answers)
            self.assertEqual(status, 2)
            self.assertEqual(writes, [])

    def test_concurrent_configuration_change_stops_credentials(self):
        module = load()
        count = 0
        def gh(kind, action, repo, name=None, value=None):
            nonlocal count
            self.assertEqual(action, 'list')
            if kind == 'secret':
                return {}
            count += 1
            return CONFIG if count == 1 else dict(CONFIG, LIVE_FLY_APP='changed')
        with patch.object(module, 'gh', side_effect=gh), \
             patch.object(module, 'discover', return_value=(['example'], ['iad'])), \
             patch.object(module.getpass, 'getpass', side_effect=['m' * 48, 'm' * 48]), \
             patch.dict(module.os.environ, {'FLY_API_TOKEN': 'private-token'}, clear=True), \
             patch('builtins.input', side_effect=['BACKED UP', 'I ACKNOWLEDGE BILLING']), \
             contextlib.redirect_stdout(io.StringIO()):
            with self.assertRaises(ValueError):
                module.install('example/repo', True)


class TransportTests(unittest.TestCase):
    def test_bounded_post_private_header_and_timeout(self):
        module = load()
        with patch.object(module.urllib.request, 'build_opener') as build:
            response = build.return_value.open.return_value.__enter__.return_value
            response.read.return_value = b'{"data":{"ok":true}}'
            self.assertEqual(module.graphql('private', module.AVAILABILITY, {'name': 'test'}), {'ok': True})
            request = build.return_value.open.call_args.args[0]
            self.assertEqual(request.full_url, 'https://api.fly.io/graphql')
            self.assertEqual(request.method, 'POST')
            self.assertEqual(request.get_header('Authorization'), 'Bearer private')
            self.assertNotIn(b'private', request.data)
            self.assertEqual(build.return_value.open.call_args.kwargs, {'timeout': 30})
            response.read.assert_called_once_with(65537)
            self.assertIsInstance(build.call_args.args[0], module.NoRedirect)

    def test_transport_fails_closed(self):
        module = load()
        for raw in [b'x' * 65537, b'bad json', b'[]', b'{}', b'{"data":null}',
                    b'{"errors":[{"message":"private"}],"data":{}}']:
            with patch.object(module.urllib.request, 'build_opener') as build:
                build.return_value.open.return_value.__enter__.return_value.read.return_value = raw
                with self.assertRaises(ValueError):
                    module.graphql('private', module.DISCOVERY)
        for error in [TimeoutError(), urllib.error.URLError('private')]:
            with patch.object(module.urllib.request, 'build_opener') as build:
                build.return_value.open.side_effect = error
                with self.assertRaises(OSError):
                    module.graphql('private', module.DISCOVERY)
        with self.assertRaises(ValueError):
            module.NoRedirect().redirect_request(None, None, 302, '', {}, 'https://evil.test')

    def test_gh_malformed_inventory_and_timeout(self):
        module = load()
        for raw in [b'{}', b'[]' * 40000, b'[{"name":"x"},{"name":"x"}]',
                    b'[{"name":"x","value":null}]', b'[null]']:
            with patch.object(module, 'run_gh', return_value=subprocess.CompletedProcess([], 0, stdout=raw)):
                with self.assertRaises(ValueError):
                    module.gh('variable', 'list', 'example/repo')
        with patch.object(module, 'run_gh', side_effect=subprocess.TimeoutExpired('gh', 30)):
            with self.assertRaises(subprocess.TimeoutExpired):
                module.gh('secret', 'list', 'example/repo')

    def test_gh_subprocess_output_bound_timeout_and_pipe_input(self):
        module = load()
        def run(code, data=None, timeout=2):
            return module.run_gh([sys.executable, '-c', code], input=data,
                                 env={}, check=True, capture_output=True, timeout=timeout)
        result = run('import sys; sys.stdout.buffer.write(sys.stdin.buffer.read())', b'private')
        self.assertEqual(result.stdout, b'private')
        with self.assertRaises(ValueError):
            run('import sys; sys.stdout.buffer.write(b"x" * 1000000)')
        with self.assertRaises(subprocess.TimeoutExpired):
            run('import time; time.sleep(5)', timeout=0.05)
        with self.assertRaises(subprocess.CalledProcessError):
            run('import sys; sys.stderr.write("private"); sys.exit(1)')
        # stderr is discarded rather than accumulated or returned in exceptions.
        self.assertEqual(run('import sys; sys.stderr.write("x" * 1000000)').stdout, b'')

    def test_private_echo_fallback_is_rejected(self):
        module = load()
        def fallback(_):
            module.warnings.warn('echo', module.getpass.GetPassWarning)
            return 'not private'
        with patch.object(module.getpass, 'getpass', side_effect=fallback):
            with self.assertRaises(module.getpass.GetPassWarning):
                module.private('private: ')


if __name__ == '__main__':
    unittest.main()
