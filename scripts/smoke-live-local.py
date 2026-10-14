#!/usr/bin/env python3
"""Isolated same-relay smoke with a real native BMWP frame and generated WASM bundle."""
import argparse
import hashlib
import importlib.util
import json
from pathlib import Path
import secrets
import subprocess
import tempfile
import time
import urllib.error
import urllib.request
import uuid

spec = importlib.util.spec_from_file_location('public_smoke', Path(__file__).with_name('smoke-live-broadcast.py'))
public = importlib.util.module_from_spec(spec)
spec.loader.exec_module(public)


def run(assets, frame):
    # Frame comes from the real Rust renderer/example, never a fabricated fixture.
    payload = frame.read_bytes()
    if not payload.startswith(b'BMWP\x02') or not 0 < len(payload) <= 8 * 1024 * 1024:
        raise ValueError('Supply a real supported bounded native presentation')
    root = Path(__file__).resolve().parent.parent
    with tempfile.TemporaryDirectory(prefix='bmux-live-isolated-') as directory:
        private = Path(directory)
        key = secrets.token_urlsafe(48)
        keyfile = private / 'publisher-key'
        keyfile.write_text(key)
        keyfile.chmod(0o600)
        ready = private / 'ready.json'
        process = subprocess.Popen(['node', str(root / 'scripts/live-dev.mjs'), '--assets', str(assets.resolve()),
            '--key-file', str(keyfile), '--state-file', str(private / 'relay.json'),
            '--ready-file', str(ready)], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        try:
            deadline = time.monotonic() + 10
            while not ready.exists():
                if process.poll() is not None or time.monotonic() >= deadline:
                    raise ValueError('Local adapter startup failed')
                time.sleep(.05)
            url = json.loads(ready.read_text())['url']
            public.smoke(url)
            opener = urllib.request.build_opener(public.NoRedirect())
            epoch = None
            def request(path, method='GET', body=None, authorized=False, revision=None):
                headers = {}
                if authorized:
                    headers['Authorization'] = 'Bearer ' + key
                    if epoch:
                        headers['X-Bmux-Epoch'] = epoch
                if revision:
                    headers.update({'X-Bmux-Revision': revision, 'Content-Type': 'application/octet-stream'})
                elif body is not None:
                    headers['Content-Type'] = 'application/json'
                req = urllib.request.Request(url + '/api/live/main/' + path, body, headers, method=method)
                try:
                    with opener.open(req, timeout=5) as response:
                        return response.status, response.read(8 * 1024 * 1024 + 1), response.headers
                except urllib.error.HTTPError as error:
                    return error.code, b'', error.headers
            status, body, _ = request('publisher/start', 'POST', json.dumps({
                'protocol': 1, 'renderer': 'bmwp-2', 'publisher_id': str(uuid.uuid4())}).encode(), True)
            if status != 200:
                raise ValueError('Publisher start failed')
            epoch = json.loads(body)['epoch']
            if request('publisher/frame', 'PUT', payload, True, '1')[0] != 204:
                raise ValueError('Native frame admission failed')
            state = json.loads(request('status')[1])
            if state['state'] != 'live' or state['epoch'] != epoch:
                raise ValueError('Native frame did not become live')
            status, delivered, _ = request('presentation?epoch=' + epoch)
            if status != 200 or hashlib.sha256(delivered).digest() != hashlib.sha256(payload).digest():
                raise ValueError('Native full-frame delivery mismatch')
            for action, expected in [('pause', 'paused'), ('resume', 'starting')]:
                if request('publisher/' + action, 'POST', b'', True)[0] != 204:
                    raise ValueError('Control transition failed')
                if json.loads(request('status')[1])['state'] != expected:
                    raise ValueError('Control state mismatch')
                if request('presentation?epoch=' + epoch)[0] != 503:
                    raise ValueError('Hidden content remained publicly accessible')
            if request('publisher/frame', 'PUT', payload, True, '2')[0] != 204:
                raise ValueError('Fresh resume frame failed')
            if request('publisher/stop', 'POST', b'', True)[0] != 204:
                raise ValueError('Stop failed')
            if json.loads(request('status')[1])['state'] != 'offline':
                raise ValueError('Stop did not clear authority')
        finally:
            process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=5)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--assets', type=Path, required=True, help='Generated immutable SHA256 directory containing manifest.json')
    parser.add_argument('--frame', type=Path, required=True, help='Real Rust-generated self-contained BMWP v2 bytes')
    args = parser.parse_args()
    try:
        run(args.assets, args.frame)
    except Exception:
        print('Isolated native-frame/WASM relay smoke failed; details suppressed')
        return 1
    print('Isolated real native frame admission/delivery, pause/resume/stop and WASM asset integrity passed')
    print('This is HTTP adapter proof, not native controller or browser Canvas visual proof')
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
