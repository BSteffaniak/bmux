#!/usr/bin/env python3
"""Mock public health and immutable asset integrity; no live network."""
import hashlib
import importlib.util
import io
import json
from pathlib import Path
import unittest
from unittest.mock import patch, Mock
import urllib.error

spec = importlib.util.spec_from_file_location('smoke', Path(__file__).with_name('smoke-live-broadcast.py'))
m = importlib.util.module_from_spec(spec)
spec.loader.exec_module(m)


class Response:
    status = 200
    def __init__(self, body): self.body = body
    def __enter__(self): return self
    def __exit__(self, *_): return False
    def read(self, size): return self.body[:size]


class SmokeTests(unittest.TestCase):
    def fixture(self):
        names = ['canvas-viewer.js', 'live-viewer.js', 'live-page.js',
                 'bmux_web_renderer.js', 'bmux_web_renderer_bg.wasm']
        files = {n: b'\0asm\x01\0\0\0' if n.endswith('.wasm') else b'// mock asset' for n in names}
        entries = [{'name': n, 'size': len(b), 'sha256': hashlib.sha256(b).hexdigest()} for n, b in files.items()]
        identity = hashlib.sha256(''.join(e['sha256'] + '\n' for e in entries).encode()).hexdigest()
        base = '/live-assets/' + identity + '/'
        routes = {'https://bmux.dev' + base + n: b for n, b in files.items()}
        routes.update({
            'https://bmux.dev/live': f'<script src="{base}live-page.js"></script>'.encode(),
            'https://live.bmux.dev/health': b'{"status":"ok","renderer":"bmwp-5"}',
            'https://bmux.dev' + base + 'manifest.json': json.dumps({
                'schema': 1, 'protocol': 1, 'renderer': 'bmwp-5',
                'bundle_sha256': identity, 'files': entries}).encode(),
        })
        opener = Mock()
        opener.open.side_effect = lambda url, timeout: Response(routes[url])
        return routes, opener, base

    def test_separate_origins_without_status_polling(self):
        _, opener, _ = self.fixture()
        with patch.object(m.urllib.request, 'build_opener', return_value=opener):
            m.smoke('https://bmux.dev')
        urls = [c.args[0] for c in opener.open.call_args_list]
        self.assertIn('https://live.bmux.dev/health', urls)
        self.assertIn('https://bmux.dev/live', urls)
        self.assertFalse(any('/status' in u or '/api/live' in u for u in urls))

    def test_components_independent(self):
        for component, excluded in [('site', '/health'), ('backend', '/live')]:
            _, opener, _ = self.fixture()
            with patch.object(m.urllib.request, 'build_opener', return_value=opener):
                m.smoke('https://bmux.dev', component)
            self.assertFalse(any(c.args[0].endswith(excluded) for c in opener.open.call_args_list))

    def test_protocol_and_corruption_rejected(self):
        for endpoint in ['https://live.bmux.dev/health', 'asset']:
            routes, opener, base = self.fixture()
            routes[endpoint if endpoint != 'asset' else 'https://bmux.dev' + base + 'live-viewer.js'] = b'corrupted'
            with patch.object(m.urllib.request, 'build_opener', return_value=opener):
                with self.assertRaises(ValueError): m.smoke('https://bmux.dev')
        routes, opener, _ = self.fixture()
        routes['https://live.bmux.dev/health'] = b'{"protocol":1,"renderer":"bmwp-3"}'
        with patch.object(m.urllib.request, 'build_opener', return_value=opener):
            with self.assertRaises(ValueError): m.smoke('https://bmux.dev')

    def test_backend_health_contract(self):
        routes, opener, _ = self.fixture()
        with patch.object(m.urllib.request, 'build_opener', return_value=opener):
            m.smoke('https://bmux.dev', component='backend')
            self.assertEqual(opener.open.call_count, 1)
            for payload in ([], None, {}, {'protocol': 1, 'renderer': 'bmwp-5'},
                            {'status': 'error', 'renderer': 'bmwp-5'},
                            {'status': 'ok', 'renderer': 'bmwp-4'},
                            {'status': 'ok', 'renderer': 'bmwp-3'}, {'status': 'ok'}):
                with self.subTest(payload=payload):
                    routes['https://live.bmux.dev/health'] = json.dumps(payload).encode()
                    with self.assertRaises(m.SmokeFailure) as caught:
                        m.smoke('https://bmux.dev', component='backend')
                    self.assertEqual(caught.exception.details['code'], 'unsupported_protocol')

    def test_old_asset_renderer_rejected(self):
        for renderer in ('bmwp-4', 'bmwp-3'):
            with self.subTest(renderer=renderer):
                routes, opener, base = self.fixture()
                url = 'https://bmux.dev' + base + 'manifest.json'
                manifest = json.loads(routes[url])
                manifest['renderer'] = renderer
                routes[url] = json.dumps(manifest).encode()
                with patch.object(m.urllib.request, 'build_opener', return_value=opener):
                    with self.assertRaises(m.SmokeFailure) as caught:
                        m.smoke('https://bmux.dev', component='site')
                self.assertEqual(caught.exception.details['code'], 'unsupported_asset_manifest')

    def test_failure_metadata_redacted(self):
        opener = Mock()
        opener.open.side_effect = urllib.error.HTTPError('https://secret', 403, 'secret',
            {'cf-ray': 'a4af00430a08d6d0-IAD', 'Location': 'secret'}, io.BytesIO(b'error code: 1010\n'))
        with patch.object(m.urllib.request, 'build_opener', return_value=opener):
            with self.assertRaises(m.SmokeFailure) as caught: m.smoke('https://bmux.dev')
        self.assertEqual(caught.exception.details['code'], 'cloudflare_1010')
        self.assertEqual(caught.exception.details['endpoint'], '/health')
        self.assertNotIn('secret', json.dumps(caught.exception.details))

    def test_unversioned_assets_rejected(self):
        routes, opener, _ = self.fixture()
        routes['https://bmux.dev/live'] = b'<script src="https://evil.example/live-page.js"></script>'
        with patch.object(m.urllib.request, 'build_opener', return_value=opener):
            with self.assertRaises(ValueError): m.smoke('https://bmux.dev')


if __name__ == '__main__': unittest.main()
