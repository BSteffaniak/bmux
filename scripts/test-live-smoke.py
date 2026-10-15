#!/usr/bin/env python3
"""Mocked public asset-discovery/integrity tests; no browser/provider/build needed."""
import hashlib
import importlib.util
import json
from pathlib import Path
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('smoke', Path(__file__).with_name('smoke-live-broadcast.py'))
m = importlib.util.module_from_spec(spec)
spec.loader.exec_module(m)


class Response:
    status = 200
    def __init__(self, body):
        self.body = body
    def __enter__(self):
        return self
    def __exit__(self, *_):
        return False
    def read(self, size):
        return self.body[:size]


class SmokeTests(unittest.TestCase):
    def fixture(self):
        order = ['canvas-viewer.js', 'live-viewer.js', 'live-page.js',
                 'bmux_web_renderer.js', 'bmux_web_renderer_bg.wasm']
        # Deliberate mock bodies test hashing only; these are not visual/native proof.
        files = {name: (b'\0asm\x01\0\0\0' if name.endswith('.wasm') else b'// mock unit asset') for name in order}
        entries = [{'name': name, 'size': len(body), 'sha256': hashlib.sha256(body).hexdigest()} for name, body in files.items()]
        version = hashlib.sha256(''.join(entry['sha256'] + '\n' for entry in entries).encode()).hexdigest()
        base = '/live-assets/' + version + '/'
        routes = {base + name: body for name, body in files.items()}
        routes.update({
            '/live': f'<script type="module" src="{base}live-page.js"></script>'.encode(),
            '/api/live/main/health': json.dumps({'protocol': 1, 'renderer': 'bmwp-3'}).encode(),
            '/api/live/main/status': json.dumps({'protocol': 1, 'renderer': 'bmwp-3', 'state': 'offline'}).encode(),
            base + 'manifest.json': json.dumps({'schema': 1, 'protocol': 1, 'renderer': 'bmwp-3',
                                                'bundle_sha256': version, 'files': entries}).encode(),
        })
        class Opener:
            def open(self, url, timeout):
                self_test.assertEqual(timeout, 10)
                return Response(routes[url.removeprefix('https://test.example')])
        self_test = self
        return routes, Opener(), base

    def test_hashed_discovery_and_integrity(self):
        _, opener, _ = self.fixture()
        with patch.object(m.urllib.request, 'build_opener', return_value=opener):
            m.smoke('https://test.example')

    def test_site_smoke_does_not_contact_backend(self):
        routes, opener, _ = self.fixture()
        del routes['/api/live/main/health']
        del routes['/api/live/main/status']
        with patch.object(m.urllib.request, 'build_opener', return_value=opener):
            m.smoke('https://test.example', 'site')

    def test_backend_smoke_does_not_contact_site(self):
        routes, opener, _ = self.fixture()
        for path in list(routes):
            if not path.startswith('/api/'):
                del routes[path]
        with patch.object(m.urllib.request, 'build_opener', return_value=opener):
            m.smoke('https://test.example', 'backend')
            routes['/api/live/main/health'] = b'{"protocol":2,"renderer":"bmwp-3"}'
            with self.assertRaises(ValueError):
                m.smoke('https://test.example', 'backend')

    def test_corruption_rejected(self):
        routes, opener, base = self.fixture()
        routes[base + 'live-viewer.js'] = b'corrupted'
        with patch.object(m.urllib.request, 'build_opener', return_value=opener):
            with self.assertRaises(ValueError):
                m.smoke('https://test.example')

    def test_external_or_unversioned_url_rejected(self):
        for src in ('https://attacker.example/live-page.js', '/live-assets/live-page.js'):
            routes, opener, _ = self.fixture()
            routes['/live'] = f'<script src="{src}"></script>'.encode()
            with patch.object(m.urllib.request, 'build_opener', return_value=opener):
                with self.assertRaises(ValueError):
                    m.smoke('https://test.example')

    def test_protocol_rejected(self):
        routes, opener, _ = self.fixture()
        routes['/api/live/main/health'] = b'{"protocol":2,"renderer":"bmwp-3"}'
        with patch.object(m.urllib.request, 'build_opener', return_value=opener):
            with self.assertRaises(ValueError):
                m.smoke('https://test.example')


if __name__ == '__main__':
    unittest.main()
