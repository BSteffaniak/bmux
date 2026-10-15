#!/usr/bin/env python3
"""Public read-only smoke; discover immutable assets, never print remote content."""
import argparse
import hashlib
from html.parser import HTMLParser
import json
import re
from urllib.parse import urlsplit
import urllib.request


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *_):
        return None


class AssetParser(HTMLParser):
    def __init__(self):
        super().__init__()
        self.bases = set()

    def handle_starttag(self, tag, attrs):
        for name, value in attrs:
            if name == 'src' and tag == 'script' and value:
                match = re.fullmatch(r'(/live-assets/[0-9a-f]{64}/)live-page\.js', value)
                if match:
                    self.bases.add(match[1])


def smoke(url, component='all'):
    opener = urllib.request.build_opener(NoRedirect())
    def get(path, limit):
        with opener.open(url + path, timeout=10) as response:
            if response.status != 200:
                raise ValueError('Unexpected HTTP status')
            body = response.read(limit + 1)
            if not body or len(body) > limit:
                raise ValueError('Response out of bounds')
            return body
    paths = ('/api/live/main/health', '/api/live/main/status') if component != 'site' else ()
    for path in paths:
        data = json.loads(get(path, 4096))
        if data.get('protocol') != 1 or data.get('renderer') != 'bmwp-3':
            raise ValueError('Unsupported public protocol')
        if path.endswith('/status') and data.get('state') not in ('offline', 'starting', 'live', 'paused'):
            raise ValueError('Unsupported public state')
    if component == 'backend':
        return
    parser = AssetParser()
    parser.feed(get('/live', 2 * 1024 * 1024).decode('utf-8'))
    if len(parser.bases) != 1:
        raise ValueError('Missing/ambiguous immutable viewer asset base')
    base = next(iter(parser.bases))
    manifest = json.loads(get(base + 'manifest.json', 8192))
    if (manifest.get('schema') != 1 or manifest.get('protocol') != 1 or
            manifest.get('renderer') != 'bmwp-3' or manifest.get('bundle_sha256') != base.split('/')[2]):
        raise ValueError('Unsupported asset manifest')
    required = {'live-page.js', 'live-viewer.js', 'canvas-viewer.js',
                'bmux_web_renderer.js', 'bmux_web_renderer_bg.wasm'}
    entries = manifest.get('files', [])
    if {item['name'] for item in entries} != required or len(entries) != len(required):
        raise ValueError('Invalid viewer asset membership')
    digests = {}
    for item in entries:
        size = item['size']
        if type(size) is not int or not 0 < size <= 16 * 1024 * 1024:
            raise ValueError('Invalid viewer size')
        body = get(base + item['name'], size)
        digest = hashlib.sha256(body).hexdigest()
        if len(body) != size or digest != item['sha256']:
            raise ValueError('Viewer asset integrity failed')
        if item['name'].endswith('.wasm') and not body.startswith(b'\0asm\x01\0\0\0'):
            raise ValueError('Invalid WASM renderer')
        digests[item['name']] = digest
    order = ['canvas-viewer.js', 'live-viewer.js', 'live-page.js',
             'bmux_web_renderer.js', 'bmux_web_renderer_bg.wasm']
    identity = hashlib.sha256(''.join(digests[name] + '\n' for name in order).encode()).hexdigest()
    if identity != manifest['bundle_sha256']:
        raise ValueError('Immutable bundle identity mismatch')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--url', required=True)
    parser.add_argument('--component', choices=('all', 'site', 'backend'), default='all')
    args = parser.parse_args()
    parsed = urlsplit(args.url)
    if (parsed.scheme != 'https' and not (parsed.scheme == 'http' and parsed.hostname in ('127.0.0.1', 'localhost'))) or parsed.username or parsed.password or parsed.query or parsed.fragment or parsed.path not in ('', '/'):
        parser.error('Use HTTPS origin or loopback HTTP; no credentials/query/path')
    try:
        smoke(args.url.rstrip('/'), args.component)
    except Exception:
        print('Public smoke failed; transport details suppressed')
        return 1
    print('Public ' + args.component + ' smoke passed')
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
