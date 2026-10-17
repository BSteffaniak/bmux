#!/usr/bin/env python3
"""Offline import-discovery tests. Never contacts Cloudflare or invokes OpenTofu."""
import contextlib
import importlib.util
import io
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location('live_imports', Path(__file__).with_name('configure-live-imports.py'))
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)
ACCOUNT = 'a' * 32
ZONE = 'b' * 32
DNS = 'c' * 32
ENV = {'CLOUDFLARE_ACCOUNT_ID': ACCOUNT, 'CLOUDFLARE_ZONE_ID': ZONE,
       'LIVE_HOSTNAME': 'bmux.dev', 'LIVE_PAGES_PROJECT': 'bmux',
       'LIVE_ENVIRONMENT': 'production', 'CLOUDFLARE_API_TOKEN': 'SECRET_TOKEN',
       'LIVE_STATE_BUCKET': 'bmux-opentofu-state'}


class FakeAPI:
    def __init__(self):
        self.data = {
            f'accounts/{ACCOUNT}/r2/buckets': [{'name': 'bmux-opentofu-state', 'storage_class': 'Standard', 'creation_date': '2026-01-01'}],
            f'accounts/{ACCOUNT}/r2/buckets/bmux-opentofu-state/lock': [],
            f'accounts/{ACCOUNT}/r2/buckets/bmux-opentofu-state/lifecycle': [],
            f'accounts/{ACCOUNT}/pages/projects': [{'name': 'bmux'}],
            f'accounts/{ACCOUNT}/pages/projects/bmux/domains': [{'name': 'bmux.dev'}],
            f'zones/{ZONE}/dns_records?name=bmux.dev': [{'id': DNS, 'name': 'bmux.dev', 'type': 'CNAME', 'content': 'bmux.pages.dev'}],
            f'accounts/{ACCOUNT}/workers/workers': [],
            f'accounts/{ACCOUNT}/workers/scripts': [],
            f'zones/{ZONE}/workers/routes': [],
        }

    def listing(self, endpoint):
        if endpoint == f'accounts/{ACCOUNT}/r2/buckets/bmux-opentofu-state':
            return self.data[f'accounts/{ACCOUNT}/r2/buckets']
        return self.data[endpoint]


class Response(io.BytesIO):
    status = 200

    def __init__(self, data, url):
        super().__init__(data)
        self.url = url

    def geturl(self):
        return self.url


class Opener:
    def __init__(self, answers):
        self.answers = iter(answers)
        self.requests = []

    def open(self, request, timeout):
        self.requests.append((request, timeout))
        answer = next(self.answers)
        if isinstance(answer, Exception):
            raise answer
        data = answer if isinstance(answer, bytes) else json.dumps(answer).encode()
        return Response(data, request.full_url)


class Tests(unittest.TestCase):
    def test_exact_discovery_and_managed_filter(self):
        found = MODULE.discover(FakeAPI(), MODULE.configuration(ENV), set())
        self.assertEqual(found, {
            'cloudflare_r2_bucket.state': ACCOUNT + '/bmux-opentofu-state/default',
            'cloudflare_pages_project.live': ACCOUNT + '/bmux',
            'cloudflare_pages_domain.live': ACCOUNT + '/bmux/bmux.dev',
            'cloudflare_dns_record.live': ZONE + '/' + DNS,
        })
        self.assertEqual(MODULE.discover(FakeAPI(), MODULE.configuration(ENV), set(found)), {})

    def test_approval_and_exact_mapping_required_no_file_on_denial(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / 'imports.tf.json'
            found = MODULE.discover(FakeAPI(), MODULE.configuration(ENV), set())
            for approved, reviewed in [(False, found), (True, {}), (True, {'cloudflare_worker.live': ACCOUNT + '/wrong'})]:
                with self.assertRaises(MODULE.ImportErrorSafe):
                    MODULE.generate(output, found, reviewed, approved)
                self.assertFalse(output.exists())
            MODULE.generate(output, found, found, True)
            self.assertEqual(output.stat().st_mode & 0o777, 0o600)
            document = json.loads(output.read_text())
            self.assertEqual({block['to']: block['id'] for block in document['import']}, found)
            with self.assertRaises(MODULE.ImportErrorSafe):
                MODULE.generate(output, found, found, True)

    def test_ambiguity_foreign_pages_and_dns(self):
        cases = [
            (f'accounts/{ACCOUNT}/pages/projects', [{'name': 'bmux'}, {'name': 'bmux'}]),
            (f'accounts/{ACCOUNT}/pages/projects/bmux/domains', [{'name': 'bmux.dev'}, {'name': 'bmux.dev'}]),
            (f'zones/{ZONE}/dns_records?name=bmux.dev', [{'name': 'bmux.dev', 'type': 'A', 'content': '1.2.3.4'}]),
            (f'zones/{ZONE}/dns_records?name=bmux.dev', [{'name': 'bmux.dev', 'type': 'CNAME', 'content': 'foreign.pages.dev'}]),
            (f'zones/{ZONE}/dns_records?name=bmux.dev', [{'name': 'bmux.dev', 'type': 'CNAME', 'content': 'bmux.pages.dev', 'id': 'SECRET_TOKEN\n::error::injection'}]),
        ]
        for endpoint, data in cases:
            api = FakeAPI()
            api.data[endpoint] = data
            with self.subTest(endpoint=endpoint), self.assertRaises(MODULE.ImportErrorSafe):
                MODULE.discover(api, MODULE.configuration(ENV), set())
        api = FakeAPI()
        api.data[f'accounts/{ACCOUNT}/pages/projects'].append({'name': 'other'})
        api.data[f'accounts/{ACCOUNT}/pages/projects/other/domains'] = [{'name': 'bmux.dev'}]
        with self.assertRaises(MODULE.ImportErrorSafe):
            MODULE.discover(api, MODULE.configuration(ENV), set())

    def test_worker_and_route_ownership_never_guessed(self):
        api = FakeAPI()
        api.data[f'accounts/{ACCOUNT}/workers/scripts'] = [{'id': 'bmux-live-production'}]
        with self.assertRaises(MODULE.ImportErrorSafe):
            MODULE.discover(api, MODULE.configuration(ENV), set())
        api = FakeAPI()
        api.data[f'accounts/{ACCOUNT}/workers/workers'] = [{'name': 'bmux-live-production', 'id': 'd' * 32}]
        with self.assertRaises(MODULE.ImportErrorSafe):
            MODULE.discover(api, MODULE.configuration(ENV), set())
        managed = {'cloudflare_worker.live'}
        # Resume after Worker creation but before its first version/deployment.
        MODULE.discover(api, MODULE.configuration(ENV), managed)
        api.data[f'zones/{ZONE}/workers/routes'] = [{'pattern': 'bmux.dev/api/live/*', 'script': 'foreign', 'id': DNS}]
        with self.assertRaises(MODULE.ImportErrorSafe):
            MODULE.discover(api, MODULE.configuration(ENV), managed)
        api.data[f'zones/{ZONE}/workers/routes'][0]['script'] = 'bmux-live-production'
        with self.assertRaises(MODULE.ImportErrorSafe):
            MODULE.discover(api, MODULE.configuration(ENV), managed)
        managed.add('cloudflare_workers_route.live')
        found = MODULE.discover(api, MODULE.configuration(ENV), managed)
        self.assertNotIn('cloudflare_workers_route.live', found)
        self.assertNotIn('cloudflare_worker.live', found)

    def test_configuration_hostile_values_are_not_echoed(self):
        for key in ENV:
            env = dict(ENV, **{key: 'SECRET_TOKEN\n::error::injection'})
            with self.subTest(key=key), self.assertRaises(MODULE.ImportErrorSafe) as error:
                MODULE.configuration(env)
            self.assertNotIn('SECRET_TOKEN', str(error.exception))

    def test_transport_get_only_bounds_and_pagination(self):
        opener = Opener([
            {'success': True, 'result': [{'id': 1}], 'result_info': {'page': 1, 'total_pages': 2}},
            {'success': True, 'result': [{'id': 2}], 'result_info': {'page': 2, 'total_pages': 2}},
        ])
        api = MODULE.Cloudflare('SECRET_TOKEN', opener)
        self.assertEqual(api.listing('accounts/test/pages/projects'), [{'id': 1}, {'id': 2}])
        for request, timeout in opener.requests:
            self.assertEqual(request.get_method(), 'GET')
            self.assertTrue(request.full_url.startswith(MODULE.BASE))
            self.assertLessEqual(timeout, 5)
        for answer in [b'x' * (MODULE.MAX_BYTES + 1), b'{truncated',
                       {'success': False, 'errors': ['SECRET_TOKEN']},
                       {'success': True, 'result': [{}] * 100},
                       {'success': True, 'result': [], 'result_info': {'page': 1, 'total_pages': 21}},
                       {'success': True, 'result': 'SECRET_TOKEN'},
                       OSError('SECRET_TOKEN')]:
            with self.subTest(answer=type(answer)), self.assertRaises(MODULE.ImportErrorSafe) as error:
                MODULE.Cloudflare('SECRET_TOKEN', Opener([answer])).listing('test')
            self.assertNotIn('SECRET_TOKEN', str(error.exception))

    def test_r2_cursor_metadata_without_page_size_echo(self):
        endpoint = f'accounts/{ACCOUNT}/r2/buckets'
        first = [{'name': str(i)} for i in range(10)]
        for terminal in ({}, {'cursor': ''}, {'cursor': None}, {'per_page': 10}):
            with self.subTest(terminal=terminal):
                opener = Opener([
                    {'success': True, 'result': {'buckets': first},
                     'result_info': {'cursor': 'next+/='}},
                    {'success': True, 'result': {'buckets': [{'name': 'last'}]},
                     'result_info': terminal},
                ])
                self.assertEqual(len(MODULE.Cloudflare('SECRET_TOKEN', opener).listing(endpoint)), 11)
                self.assertIn('cursor=next%2B%2F%3D', opener.requests[1][0].full_url)
        for info in ({}, {'cursor': ''}, {'cursor': None}, {'cursor': 42},
                     {'cursor': 'x', 'per_page': True}, {'cursor': 'x', 'per_page': '10'}):
            with self.subTest(invalid=info), self.assertRaises(MODULE.ImportErrorSafe):
                MODULE.Cloudflare('SECRET_TOKEN', Opener([
                    {'success': True, 'result': {'buckets': first}, 'result_info': info},
                ])).listing(endpoint)

    def test_endpoint_specific_pagination_contracts(self):
        endpoint = f'accounts/{ACCOUNT}/r2/buckets'
        answers = [
            {'success': True, 'result': {'buckets': [{'name': str(i)} for i in range(10)]},
             'result_info': {'per_page': 10, 'cursor': 'next+/='}},
            {'success': True, 'result': {'buckets': [{'name': 'last'}]}},
        ]
        opener = Opener(answers)
        self.assertEqual(len(MODULE.Cloudflare('SECRET_TOKEN', opener).listing(endpoint)), 11)
        self.assertIn('cursor=next%2B%2F%3D', opener.requests[1][0].full_url)
        self.assertNotIn('page=', opener.requests[0][0].full_url.replace('per_page=', 'size='))
        for suffix in ('workers/scripts', 'workers/routes', 'r2/buckets/b/lock', 'r2/buckets/b/lifecycle'):
            result = {'rules': [{}] * 15} if suffix.endswith(('/lock', '/lifecycle')) else [{}] * 15
            opener = Opener([{'success': True, 'result': result}])
            self.assertEqual(len(MODULE.Cloudflare('SECRET_TOKEN', opener).listing('accounts/a/' + suffix)), 15)
            self.assertNotIn('?', opener.requests[0][0].full_url)
        opener = Opener([{'success': True, 'result': [],
                          'result_info': {'page': 1, 'per_page': 10, 'total_count': 0}}])
        MODULE.Cloudflare('SECRET_TOKEN', opener).listing('accounts/a/pages/projects')
        self.assertIn('per_page=10', opener.requests[0][0].full_url)
        with self.assertRaises(MODULE.ImportErrorSafe):
            MODULE.Cloudflare('SECRET_TOKEN', Opener([{'success': True, 'result': [{}] * 10}])).listing('test')
        repeat = {'success': True, 'result': {'buckets': [{}] * 10},
                  'result_info': {'per_page': 10, 'cursor': 'same'}}
        with self.assertRaises(MODULE.ImportErrorSafe):
            MODULE.Cloudflare('SECRET_TOKEN', Opener([repeat, repeat])).listing(endpoint)
        from urllib.error import HTTPError
        opener = Opener([HTTPError('SECRET_TOKEN', 400, 'SECRET_TOKEN', {}, None)])
        with self.assertRaises(MODULE.ImportErrorSafe) as error:
            MODULE.Cloudflare('SECRET_TOKEN', opener).listing('accounts/a/pages/projects')
        self.assertIn('HTTP 400', str(error.exception))
        self.assertNotIn('SECRET_TOKEN', str(error.exception))

    def test_workers_count_pagination_and_r2_terminal_shapes(self):
        opener = Opener([
            {'success': True, 'result': [{'id': i} for i in range(10)],
             'result_info': {'page': 1, 'per_page': 10, 'count': 10, 'total_count': 11}},
            {'success': True, 'result': [{'id': 10}],
             'result_info': {'page': 2, 'per_page': 10, 'count': 1, 'total_count': 11}},
        ])
        self.assertEqual(len(MODULE.Cloudflare('SECRET_TOKEN', opener).listing('accounts/a/workers/workers')), 11)
        self.assertIn('page=2', opener.requests[1][0].full_url)
        endpoint = f'accounts/{ACCOUNT}/r2/buckets'
        for info in ({'cursor': 'x', 'per_page': 11}, {'cursor': 42, 'per_page': 10}):
            with self.subTest(info=info), self.assertRaises(MODULE.ImportErrorSafe):
                MODULE.Cloudflare('SECRET_TOKEN', Opener([{'success': True, 'result': {'buckets': []},
                                                          'result_info': info}])).listing(endpoint)
        full = {'success': True, 'result': {'buckets': [{}] * 10}}
        with self.assertRaises(MODULE.ImportErrorSafe):
            MODULE.Cloudflare('SECRET_TOKEN', Opener([full])).listing(endpoint)
        detail = {'name': 'bmux-opentofu-state', 'creation_date': '2026-01-01', 'storage_class': 'Standard'}
        opener = Opener([{'success': True, 'result': detail}])
        self.assertEqual(MODULE.Cloudflare('SECRET_TOKEN', opener).listing(endpoint + '/bmux-opentofu-state'), [detail])
        self.assertNotIn('?', opener.requests[0][0].full_url)

    def test_bucket_detail_must_match_inventory_generation(self):
        api = FakeAPI()
        original_listing = api.listing
        def listing(endpoint):
            if endpoint == f'accounts/{ACCOUNT}/r2/buckets/bmux-opentofu-state':
                return [{'name': 'bmux-opentofu-state', 'storage_class': 'Standard', 'creation_date': 'changed'}]
            return original_listing(endpoint)
        api.listing = listing
        with self.assertRaises(MODULE.ImportErrorSafe):
            MODULE.discover(api, MODULE.configuration(ENV), set())

    def test_redirect_and_deadline_fail_closed(self):
        with self.assertRaises(MODULE.ImportErrorSafe):
            MODULE.NoRedirect().redirect_request(None, None, 302, 'SECRET_TOKEN', {}, 'https://evil.invalid')
        ticks = iter([0, 31])
        api = MODULE.Cloudflare('SECRET_TOKEN', Opener([]), clock=lambda: next(ticks))
        with self.assertRaises(MODULE.ImportErrorSafe):
            api.listing('test')
        opener = Opener([{'success': True, 'result': [], 'result_info': {'page': 2, 'total_pages': 2}}])
        with self.assertRaises(MODULE.ImportErrorSafe):
            MODULE.Cloudflare('SECRET_TOKEN', opener).listing('test')

    def test_dns_mail_records_do_not_compete_with_pages_but_addresses_do(self):
        api = FakeAPI()
        endpoint = f'zones/{ZONE}/dns_records?name=bmux.dev'
        cname = api.data[endpoint][0]
        api.data[endpoint] = [cname, {'name': 'bmux.dev', 'type': 'MX'},
                              {'name': 'bmux.dev', 'type': 'TXT'}]
        found = MODULE.discover(api, MODULE.configuration(ENV), set())
        self.assertEqual(found['cloudflare_dns_record.live'], ZONE + '/' + DNS)
        for record_type in ('A', 'AAAA', 'CNAME'):
            with self.subTest(record_type=record_type), self.assertRaises(MODULE.ImportErrorSafe):
                api.data[endpoint] = [cname, {'name': 'bmux.dev', 'type': record_type}]
                MODULE.discover(api, MODULE.configuration(ENV), set())

    def test_only_exact_preserved_default_lifecycle_is_adoptable(self):
        baseline = {'id': 'Default Multipart Abort Rule', 'enabled': True, 'conditions': {},
                    'abortMultipartUploadsTransition': {'condition': {'type': 'Age', 'maxAge': 604800}}}
        endpoint = f'accounts/{ACCOUNT}/r2/buckets/bmux-opentofu-state/lifecycle'
        api = FakeAPI()
        api.data[endpoint] = [baseline]
        self.assertIn('cloudflare_r2_bucket.state', MODULE.discover(api, MODULE.configuration(ENV), set()))
        import copy
        variants = []
        for field, value in [('id', 'custom'), ('enabled', False), ('conditions', {'prefix': 'private/'})]:
            changed = copy.deepcopy(baseline)
            changed[field] = value
            variants.append([changed])
        changed = copy.deepcopy(baseline)
        changed['abortMultipartUploadsTransition']['condition']['maxAge'] = 86400
        variants.extend(([changed], [baseline, {'id': 'custom'}], [dict(baseline, deleteObjectsTransition={})]))
        for rules in variants:
            with self.subTest(rules=rules), self.assertRaises(MODULE.ImportErrorSafe):
                api.data[endpoint] = rules
                MODULE.discover(api, MODULE.configuration(ENV), set())
        # Prevent code/config divergence from silently shortening the preserved rule.
        config = (Path(__file__).resolve().parent.parent / 'infra/live-terraform/state.tf').read_text()
        self.assertIn('id      = "Default Multipart Abort Rule"', config)
        self.assertIn('max_age = 604800', config)

    def test_r2_bucket_and_unmanaged_policy_guards(self):
        config = MODULE.configuration(ENV)
        endpoint = f'accounts/{ACCOUNT}/r2/buckets'
        for rows in ([], [{'name': config['bucket']}]):
            api = FakeAPI()
            api.data[endpoint] = rows
            with self.assertRaises(MODULE.ImportErrorSafe):
                MODULE.discover(api, config, set())
        for suffix, address in (('lock', 'cloudflare_r2_bucket_lock.state'),
                                ('lifecycle', 'cloudflare_r2_bucket_lifecycle.state')):
            api = FakeAPI()
            api.data[f'{endpoint}/{config["bucket"]}/{suffix}'] = [{'id': 'unrelated'}]
            with self.assertRaises(MODULE.ImportErrorSafe):
                MODULE.discover(api, config, set())
            self.assertIn('cloudflare_r2_bucket.state', MODULE.discover(api, config, {address}))

    def test_r2_inventory_and_rules_response_shapes(self):
        for suffix, result in (('/bmux-opentofu-state/lock', {'rules': []}),
                               ('/bmux-opentofu-state/lifecycle', {'rules': []})):
            api = MODULE.Cloudflare('SECRET_TOKEN', Opener([{'success': True, 'result': result}]))
            self.assertIsInstance(api.listing(f'accounts/{ACCOUNT}/r2/buckets{suffix}'), list)
        api = MODULE.Cloudflare('SECRET_TOKEN', Opener([{'success': True, 'result': {'buckets': [], 'cursor': 'more'}}]))
        with self.assertRaises(MODULE.ImportErrorSafe):
            api.listing(f'accounts/{ACCOUNT}/r2/buckets')

    def test_private_output_and_input_validation(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            artifact = root / 'artifact'
            artifact.mkdir()
            source = Path(__file__).resolve().parent.parent / 'infra/live-terraform/main.tf'
            (artifact / 'main.tf').write_text(source.read_text())
            (artifact / 'state.tf').write_text(source.with_name('state.tf').read_text())
            output = root / 'imports.tf.json'
            self.assertEqual(MODULE.paths(str(artifact), str(output)), output)
            root.chmod(0o755)
            with self.assertRaises(MODULE.ImportErrorSafe):
                MODULE.paths(str(artifact), str(output))
            root.chmod(0o700)
            target = root / 'target'
            target.write_text('keep')
            output.symlink_to(target)
            with self.assertRaises(MODULE.ImportErrorSafe):
                MODULE.paths(str(artifact), str(output))
            with self.assertRaises(MODULE.ImportErrorSafe):
                MODULE.generate(output, {}, {}, False)
            self.assertEqual(target.read_text(), 'keep')
            input_path = root / 'managed.json'
            input_path.write_text('{"a":1,"a":2}')
            with self.assertRaises(MODULE.ImportErrorSafe):
                MODULE.read_json(input_path)
            with self.assertRaises(MODULE.ImportErrorSafe):
                MODULE.managed_addresses(['x', 'x'])
            with self.assertRaises(MODULE.ImportErrorSafe):
                MODULE.reviewed_imports({'evil\n::error::': 'SECRET_TOKEN'})

    def test_empty_paginated_list_and_changing_pages(self):
        empty = {'success': True, 'result': [], 'result_info': {'page': 1, 'total_pages': 0}}
        self.assertEqual(MODULE.Cloudflare('SECRET_TOKEN', Opener([empty])).listing('test'), [])
        changed = [
            {'success': True, 'result': [], 'result_info': {'page': 1, 'total_pages': 2}},
            {'success': True, 'result': [], 'result_info': {'page': 2, 'total_pages': 3}},
        ]
        with self.assertRaises(MODULE.ImportErrorSafe):
            MODULE.Cloudflare('SECRET_TOKEN', Opener(changed)).listing('test')

    def test_no_existing_resources_needs_no_approval(self):
        api = FakeAPI()
        api.data[f'accounts/{ACCOUNT}/pages/projects'] = []
        api.data[f'zones/{ZONE}/dns_records?name=bmux.dev'] = []
        found = MODULE.discover(api, MODULE.configuration(ENV), set(MODULE.STATE_ADDRESSES))
        self.assertEqual(found, {})
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / 'imports.tf.json'
            MODULE.generate(output, found, {}, False)
            self.assertEqual(json.loads(output.read_text()), {'import': []})

    def test_hostile_discovered_project_never_reaches_output(self):
        api = FakeAPI()
        api.data[f'accounts/{ACCOUNT}/pages/projects'] = [{'name': 'SECRET_TOKEN\n::error::'}]
        with self.assertRaises(MODULE.ImportErrorSafe) as error:
            MODULE.discover(api, MODULE.configuration(ENV), set())
        self.assertNotIn('SECRET_TOKEN', str(error.exception))

    def test_cli_denial_prints_safe_review_and_writes_nothing(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            managed = root / 'managed.json'
            managed.write_text('[]')
            output = root / 'imports.tf.json'
            stdout, stderr = io.StringIO(), io.StringIO()
            with patch.dict(os.environ, ENV, clear=True), patch.object(MODULE, 'Cloudflare', return_value=FakeAPI()), patch.object(MODULE, 'paths', return_value=output), contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
                result = MODULE.main(['--artifact', str(root), '--output', str(output), '--managed', str(managed)])
            self.assertEqual(result, 1)
            self.assertFalse(output.exists())
            summary = json.loads(stdout.getvalue())
            self.assertRegex(summary['fingerprint'], '^[a-f0-9]{64}$')
            self.assertNotIn('SECRET_TOKEN', stdout.getvalue() + stderr.getvalue())


if __name__ == '__main__':
    unittest.main()
