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
             patch('builtins.input', side_effect=answers if answers is not None else ['1', '1', 'bmux-test', 'BACKED UP', 'I ACKNOWLEDGE BILLING']) as public, \
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
        status, writes, calls, *_ = self.run_setup()
        self.assertEqual(status, 0)
        self.assertEqual(len(writes), 5)
        self.assertIn(('secret', 'FLY_API_TOKEN', b'private-token'), writes)
        self.assertIn(('secret', 'LIVE_METRICS_KEY_SHA256', hashlib.sha256(b'm' * 48).hexdigest().encode()), writes)
        for args, _ in calls:
            self.assertEqual(args[0], 'gh')
            self.assertIn(args[1], ['secret', 'variable'])
            if args[1] == 'secret':
                self.assertIn('Production', args)
            else:
                self.assertNotIn('--env', args)

    def test_report_only_no_credentials_or_discovery(self):
        status, writes, calls, api, private, public, out, _ = self.run_setup(approve=False)
        self.assertEqual(status, 0)
        self.assertEqual(writes, [])
        self.assertEqual(len(calls), 2)
        api.assert_not_called()
        private.assert_not_called()
        public.assert_not_called()
        self.assertIn('LIVE_FLY_REGION', out)
        self.assertNotIn('Public Fly', out)

    def test_manual_next_step_only_when_ready_never_dispatched(self):
        command = 'gh workflow run live-promote.yml --repo BSteffaniak/bmux --ref master -f migrate=true'
        for approve in [False, True]:
            result = self.run_setup(variables=CONFIG,
                secrets=['FLY_API_TOKEN', 'LIVE_METRICS_KEY_SHA256'], approve=approve)
            self.assertEqual(result[0], 0)
            self.assertIn(command, result[6].splitlines())
            self.assertIn('Review the workflow preview, then approve production-approval', result[6])
            self.assertIn('no deployment spending', result[6])
            self.assertEqual(len(result[2]), 2)
        result = self.run_setup()
        self.assertEqual(result[0], 0)
        self.assertIn(command, result[6].splitlines())
        for options in [dict(approve=False), dict(available=False),
                        dict(fail_set='FLY_API_TOKEN'), dict(answers=[''])]:
            result = self.run_setup(**options)
            self.assertNotIn(command, result[6])
        module = load()
        with contextlib.redirect_stdout(io.StringIO()) as out:
            module.next_step('example/repo')
        self.assertIn('gh workflow run live-promote.yml --repo example/repo --ref master -f migrate=true',
                      out.getvalue().splitlines())

    def test_all_existing_no_prompts_or_overwrite(self):
        status, writes, calls, api, private, public, *_ = self.run_setup(
            variables=CONFIG, secrets=['FLY_API_TOKEN', 'LIVE_METRICS_KEY_SHA256'])
        self.assertEqual(status, 0)
        self.assertEqual(writes, [])
        self.assertEqual(len(calls), 2)
        api.assert_not_called()
        private.assert_not_called()
        public.assert_not_called()

    def test_existing_installed_token_private_discovery_only(self):
        status, writes, _, _, prompt, *_ = self.run_setup(
            variables={'LIVE_FLY_ORG': 'example', 'LIVE_FLY_REGION': 'iad'},
            secrets=['FLY_API_TOKEN', 'LIVE_METRICS_KEY_SHA256'],
            private=['private-token'], answers=['bmux-test', 'I ACKNOWLEDGE BILLING'])
        self.assertEqual(status, 0)
        self.assertEqual(writes, [('variable', 'LIVE_FLY_APP', b'bmux-test')])
        self.assertIn('discovery only', prompt.call_args.args[0])

    def test_environment_token_not_rotated(self):
        status, writes, _, _, prompt, *_ = self.run_setup(
            variables=CONFIG, secrets=['FLY_API_TOKEN'], env={'FLY_API_TOKEN': 'private-token'},
            private=['m' * 48, 'm' * 48], answers=['BACKED UP', 'I ACKNOWLEDGE BILLING'])
        self.assertEqual(status, 0)
        self.assertEqual([w[1] for w in writes], ['LIVE_METRICS_KEY_SHA256'])
        self.assertEqual(prompt.call_count, 2)

    def test_empty_explicit_value_not_replaced(self):
        status, writes, *_ = self.run_setup(variables=dict(CONFIG, LIVE_FLY_ORG=''))
        self.assertEqual(status, 2)
        self.assertEqual(writes, [])

    def test_collision_and_non_boolean_availability(self):
        for available in [False, None, 'true', 1, {}, []]:
            with self.subTest(available=available):
                status, writes, *_ = self.run_setup(available=available)
                self.assertEqual(status, 2)
                self.assertEqual(writes, [])

    def test_cancel_at_each_public_prompt_and_private_prompt(self):
        for answers in [[''], ['1', ''], ['1', '1', ''],
                        ['1', '1', 'bmux-test', 'no'],
                        ['1', '1', 'bmux-test', 'BACKED UP', 'no']]:
            with self.subTest(answers=answers):
                status, writes, *_ = self.run_setup(answers=answers)
                self.assertEqual(status, 2)
                self.assertEqual(writes, [])
        for value in [KeyboardInterrupt(), EOFError(), '', 'bad\ntoken']:
            status, writes, *_ = self.run_setup(private=[value])
            self.assertEqual(status, 2)
            self.assertEqual(writes, [])

    def test_metrics_never_generated_and_backup_required(self):
        for values in [['private-token', 'short', 'short'],
                       ['private-token', 'm' * 48, 'n' * 48]]:
            status, writes, *_ = self.run_setup(private=values)
            self.assertEqual(status, 2)
            self.assertEqual(writes, [])

    def test_malformed_and_incomplete_inventory(self):
        bad = [[], {}, {'organizations': None},
               dict(DISCOVERY, organizations={'nodes': [], 'pageInfo': {'hasNextPage': False}}),
               dict(DISCOVERY, organizations={'nodes': [{'slug': 'example'}], 'pageInfo': {'hasNextPage': True}}),
               dict(DISCOVERY, organizations={'nodes': [{'slug': 'example'}] * 101, 'pageInfo': {'hasNextPage': False}}),
               dict(DISCOVERY, platform={'regions': [{'code': 'iad', 'deprecated': 'false'}]}),
               dict(DISCOVERY, platform={'regions': [{'code': 'iad', 'deprecated': True}]}),
               dict(DISCOVERY, platform={'regions': [{'code': 'iad', 'deprecated': False}] * 501})]
        for discovery in bad:
            with self.subTest(discovery=str(discovery)[:100]):
                status, writes, *_ = self.run_setup(discovery=discovery)
                self.assertEqual(status, 2)
                self.assertEqual(writes, [])

    def test_discovery_timeout_authentication_failure_and_malformed_gh_stop(self):
        for error in [TimeoutError(), urllib.error.HTTPError('private', 401, 'private', {}, None)]:
            status, writes, *_ = self.run_setup(discovery=error)
            self.assertEqual(status, 2)
            self.assertEqual(writes, [])
        module = load()
        for error in [subprocess.TimeoutExpired('gh', 30),
                      subprocess.CalledProcessError(1, ['gh'], stderr=b'private-token')]:
            with patch.object(module, 'run_gh', side_effect=error) as run, \
                 patch.object(module, 'graphql') as api, \
                 patch('sys.argv', ['setup-live-fly.py', '--approve']), \
                 contextlib.redirect_stderr(io.StringIO()) as err:
                with self.assertRaises(SystemExit) as stopped:
                    module.main()
                self.assertEqual(stopped.exception.code, 2)
                self.assertEqual(run.call_count, 1)
                api.assert_not_called()
                self.assertNotIn('private-token', err.getvalue())

    def test_partial_failure_is_not_success_and_rerun_preserves_values(self):
        status, writes, _, _, _, _, out, err = self.run_setup(fail_set='FLY_API_TOKEN')
        self.assertEqual(status, 2)
        self.assertEqual(len(writes), 3)
        self.assertIn('Partial installation is possible', err)
        self.assertNotIn('configuration installed', out)
        status, writes, *_ = self.run_setup(variables=CONFIG,
            answers=['BACKED UP', 'I ACKNOWLEDGE BILLING'])
        self.assertEqual(status, 0)
        self.assertEqual(len(writes), 2)

    def test_recheck_never_overwrites_concurrent_value(self):
        module = load()
        for kind in ['secret', 'variable']:
            secrets = {'FLY_API_TOKEN': True, 'LIVE_METRICS_KEY_SHA256': True}
            variables = dict(CONFIG)
            if kind == 'secret':
                secrets.pop('LIVE_METRICS_KEY_SHA256')
            else:
                variables.pop('LIVE_FLY_APP')
            def gh(k, action, repo, name=None, value=None):
                self.assertEqual(action, 'list')  # any attempted write fails this test
                if k == 'variable':
                    return variables if gh.initial or kind == 'secret' else dict(CONFIG, LIVE_FLY_APP='other-app')
                result = secrets if gh.initial else dict.fromkeys(module.SECRETS, True)
                return result
            gh.initial = True
            def acknowledge(_):
                gh.initial = False
                return 'I ACKNOWLEDGE BILLING'
            answers = (['BACKED UP'] if kind == 'secret' else ['bmux-test'])
            with patch.object(module, 'gh', side_effect=gh), \
                 patch.object(module, 'graphql', side_effect=[DISCOVERY, {'appNameAvailable': True}]), \
                 patch.object(module.getpass, 'getpass', side_effect=['m' * 48, 'm' * 48]), \
                 patch.dict(module.os.environ, {'FLY_API_TOKEN': 'private-token'}, clear=True), \
                 patch('builtins.input', side_effect=lambda prompt: answers.pop(0) if answers else acknowledge(prompt)), \
                 contextlib.redirect_stdout(io.StringIO()):
                if kind == 'variable':
                    with self.assertRaises(ValueError):
                        module.install('example/repo', True)
                else:
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
