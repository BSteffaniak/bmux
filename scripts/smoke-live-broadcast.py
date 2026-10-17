#!/usr/bin/env python3
"""Public read-only smoke; discover immutable assets, never print remote content."""
import argparse
import hashlib
from html.parser import HTMLParser
import json
import re
import socket
import time
import urllib.error
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


class SmokeFailure(ValueError):
    """Only locally constructed metadata; never exception strings or bodies."""
    def __init__(self, code, endpoint, **metadata):
        super().__init__(code)
        self.details = dict(code=code, endpoint=endpoint, **metadata)


def safe_endpoint(path):
    if path in ('/health', '/live'):
        return path
    if re.fullmatch(r'/live-assets/[0-9a-f]{64}/[a-zA-Z0-9_.-]+', path):
        return path
    return '[invalid-endpoint]'


def response_metadata(response):
    headers = getattr(response, 'headers', {})
    ray = headers.get('cf-ray', '')
    return {'cf_ray': ray if re.fullmatch(r'[a-fA-F0-9]{16,32}-[A-Z]{3}', ray) else None}


def smoke(url, component='all', report=None, relay_url='https://live.bmux.dev'):
    opener = urllib.request.build_opener(NoRedirect())
    opener.addheaders = [('User-Agent', 'bmux-public-smoke/1')]
    def emit(**fields):
        if report:
            report(fields)
    def get(path, limit):
        endpoint = safe_endpoint(path)
        started = time.monotonic()
        emit(event='request', endpoint=endpoint)
        try:
            with opener.open((relay_url if path == '/health' else url) + path, timeout=10) as response:
                metadata = dict(http_status=response.status, **response_metadata(response))
                if response.status != 200:
                    raise SmokeFailure('unexpected_http_status', endpoint, **metadata)
                body = response.read(limit + 1)
                if not body or len(body) > limit:
                    raise SmokeFailure('response_out_of_bounds', endpoint, **metadata)
                emit(event='response', endpoint=endpoint, bytes=len(body),
                     elapsed_ms=round((time.monotonic() - started) * 1000), **metadata)
                return body
        except urllib.error.HTTPError as error:
            # Error 1010 is a fixed Cloudflare code, not arbitrary response text.
            # Do not log the body, Location, exception string, or other headers.
            with error:
                body = error.read(256)
            code = 'cloudflare_1010' if body.strip() == b'error code: 1010' else 'http_error'
            raise SmokeFailure(code, endpoint, http_status=error.code,
                               **response_metadata(error)) from None
        except (TimeoutError, socket.timeout):
            raise SmokeFailure('timeout', endpoint) from None
        except urllib.error.URLError:
            raise SmokeFailure('transport_error', endpoint) from None
    def get_json(path, limit):
        try:
            return json.loads(get(path, limit))
        except (json.JSONDecodeError, UnicodeDecodeError):
            raise SmokeFailure('invalid_json', safe_endpoint(path)) from None
    if component != 'site':
        data = get_json('/health', 4096)
        if not isinstance(data, dict) or data.get('status') != 'ok' or data.get('renderer') != 'bmwp-4':
            raise SmokeFailure('unsupported_protocol', '/health')
    if component == 'backend':
        return
    parser = AssetParser()
    parser.feed(get('/live', 2 * 1024 * 1024).decode('utf-8'))
    if len(parser.bases) != 1:
        raise SmokeFailure('missing_or_ambiguous_viewer_assets', '/live')
    base = next(iter(parser.bases))
    manifest = get_json(base + 'manifest.json', 8192)
    if (manifest.get('schema') != 1 or manifest.get('protocol') != 1 or
            manifest.get('renderer') != 'bmwp-4' or manifest.get('bundle_sha256') != base.split('/')[2]):
        raise SmokeFailure('unsupported_asset_manifest', base + 'manifest.json')
    required = {'live-page.js', 'live-viewer.js', 'canvas-viewer.js',
                'bmux_web_renderer.js', 'bmux_web_renderer_bg.wasm'}
    entries = manifest.get('files', [])
    if {item['name'] for item in entries} != required or len(entries) != len(required):
        raise SmokeFailure('invalid_asset_membership', base + 'manifest.json')
    digests = {}
    for item in entries:
        size = item['size']
        if type(size) is not int or not 0 < size <= 16 * 1024 * 1024:
            raise SmokeFailure('invalid_asset_size', base + 'manifest.json')
        body = get(base + item['name'], size)
        digest = hashlib.sha256(body).hexdigest()
        if len(body) != size or digest != item['sha256']:
            raise SmokeFailure('asset_integrity_failed', safe_endpoint(base + item['name']))
        if item['name'].endswith('.wasm') and not body.startswith(b'\0asm\x01\0\0\0'):
            raise SmokeFailure('invalid_wasm', safe_endpoint(base + item['name']))
        digests[item['name']] = digest
    order = ['canvas-viewer.js', 'live-viewer.js', 'live-page.js',
             'bmux_web_renderer.js', 'bmux_web_renderer_bg.wasm']
    identity = hashlib.sha256(''.join(digests[name] + '\n' for name in order).encode()).hexdigest()
    if identity != manifest['bundle_sha256']:
        raise SmokeFailure('bundle_identity_mismatch', base + 'manifest.json')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--url', required=True)
    parser.add_argument('--relay-url', default='https://live.bmux.dev')
    parser.add_argument('--component', choices=('all', 'site', 'backend'), default='all')
    args = parser.parse_args()
    for origin in (args.url, args.relay_url):
        parsed = urlsplit(origin)
        if (parsed.scheme != 'https' and not (parsed.scheme == 'http' and parsed.hostname in ('127.0.0.1', 'localhost'))) or parsed.username or parsed.password or parsed.query or parsed.fragment or parsed.path not in ('', '/'):
            parser.error('Use HTTPS origin or loopback HTTP; no credentials/query/path')
    def report(fields):
        print(json.dumps(fields, sort_keys=True), flush=True)
    try:
        smoke(args.url.rstrip('/'), args.component, report=report, relay_url=args.relay_url.rstrip('/'))
    except SmokeFailure as error:
        report(dict(event='failure', component=args.component, **error.details))
        return 1
    except Exception as error:
        # Schema/integrity errors must not stringify remotely influenced values.
        report(dict(event='failure', component=args.component, code='validation_failed',
                    category=type(error).__name__))
        return 1
    report(dict(event='passed', component=args.component))
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
