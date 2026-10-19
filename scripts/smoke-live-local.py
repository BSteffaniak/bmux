#!/usr/bin/env python3
"""Owned Rust relay / TLS acceptance support and real BMWP5 local smoke (no builds)."""
import argparse
import base64
import hashlib
import http.server
import importlib.util
import json
import os
from pathlib import Path
import secrets
import select
import socket
import ssl
import struct
import subprocess
import tempfile
import threading
import time
import urllib.request
import uuid

ROOT = Path(__file__).resolve().parent.parent
ORIGIN = 'https://bmux.dev'
MAX_BYTES = 8 * 1024 * 1024


def wait(predicate, seconds=10):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        result = predicate()
        if result:
            return result
        time.sleep(.05)
    raise AssertionError('owned local acceptance deadline expired')


def verify_assets(assets):
    """Verify exactly the immutable format emitted by build-web-renderer.sh."""
    assets = Path(assets)
    manifest = json.loads((assets / 'manifest.json').read_text())
    assert manifest['schema'] == 1 and manifest['protocol'] == 1 and manifest['renderer'] == 'bmwp-5'
    expected = ['canvas-viewer.js', 'live-viewer.js', 'live-page.js', 'bmux_web_renderer.js', 'bmux_web_renderer_bg.wasm']
    assert [entry['name'] for entry in manifest['files']] == expected
    digests = []
    for entry in manifest['files']:
        data = (assets / entry['name']).read_bytes()
        digest = hashlib.sha256(data).hexdigest()
        assert len(data) == entry['size'] and digest == entry['sha256'], 'immutable asset integrity mismatch'
        digests.append(digest)
    bundle = hashlib.sha256(('\n'.join(digests) + '\n').encode()).hexdigest()
    assert bundle == manifest['bundle_sha256'] == assets.name, 'bundle identity mismatch'
    return manifest


class WebSocket:
    """Bounded masked RFC6455 client; TLS identity is this harness's temporary CA."""
    def __init__(self, port, certificate, path, headers=None):
        context = ssl.create_default_context(cafile=str(certificate))
        self.socket = context.wrap_socket(socket.create_connection(('127.0.0.1', port), timeout=5), server_hostname='bmux.dev')
        self.pending = bytearray()
        key = base64.b64encode(os.urandom(16)).decode()
        request = f'GET {path} HTTP/1.1\r\nHost: bmux.dev\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n'
        request += ''.join(f'{name}: {value}\r\n' for name, value in (headers or {}).items()) + '\r\n'
        self.socket.sendall(request.encode())
        while b'\r\n\r\n' not in self.pending:
            chunk = self.socket.recv(4096)
            if not chunk or len(self.pending) + len(chunk) > 16384:
                raise AssertionError('invalid WS handshake')
            self.pending.extend(chunk)
        head, rest = bytes(self.pending).split(b'\r\n\r\n', 1)
        self.pending = bytearray(rest)
        assert head.startswith(b'HTTP/1.1 101 '), 'WS rejected'
        expected = base64.b64encode(hashlib.sha1((key + '258EAFA5-E914-47DA-95CA-C5AB0DC85B11').encode()).digest())
        assert b'sec-websocket-accept: ' + expected.lower() in head.lower()

    def read(self, size):
        while len(self.pending) < size:
            data = self.socket.recv(min(65536, size - len(self.pending)))
            if not data:
                raise EOFError('WS closed')
            self.pending.extend(data)
        data = bytes(self.pending[:size]); del self.pending[:size]
        return data

    def send(self, data):
        opcode = 2 if isinstance(data, bytes) else 1
        payload = data if isinstance(data, bytes) else json.dumps(data).encode()
        self.send_raw(opcode, payload)

    def send_raw(self, opcode, payload):
        assert len(payload) <= MAX_BYTES
        mask = os.urandom(4)
        size = len(payload)
        header = bytes([128 | opcode, 128 | min(size, 126)]) if size < 126 else bytes([128 | opcode, 254]) + struct.pack('!H', size) if size <= 65535 else bytes([128 | opcode, 255]) + struct.pack('!Q', size)
        self.socket.sendall(header + mask + bytes(byte ^ mask[index % 4] for index, byte in enumerate(payload)))

    def receive(self):
        result = bytearray(); kind = None
        while True:
            first, second = self.read(2)
            assert first & 0x70 == 0 and not second & 128
            size = second & 127
            if size == 126: size = struct.unpack('!H', self.read(2))[0]
            elif size == 127: size = struct.unpack('!Q', self.read(8))[0]
            assert size <= MAX_BYTES and len(result) + size <= MAX_BYTES
            payload = self.read(size); opcode = first & 15
            if opcode == 8: raise EOFError('WS closed')
            if opcode == 9: self.send_raw(10, payload); continue
            if opcode == 10: continue
            if opcode in (1, 2): assert kind is None; kind = opcode
            else: assert opcode == 0 and kind is not None
            result.extend(payload)
            if first & 128:
                return bytes(result) if kind == 2 else json.loads(result)

    def close(self):
        self.socket.close()


class RelayStack:
    """All processes, certificate, keys and authority are temporary and owned."""
    def __init__(self, relay, root, assets, site=None):
        self.publisher = self.process = self.server = self.log = None
        try:
            self._start(relay, root, assets, site)
        except BaseException:
            self.close()
            raise

    def _start(self, relay, root, assets, site=None):
        verify_assets(assets)
        self.root, self.assets, self.site = Path(root), Path(assets).resolve(), Path(site).resolve() if site else None
        self.key, self.metrics_key = secrets.token_urlsafe(48), secrets.token_urlsafe(48)
        self.epoch, self.fence, self.publisher = None, None, None
        self.network = []
        self.last_snapshot = -float('inf')
        with socket.socket() as probe:
            probe.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
            probe.bind(('127.0.0.1', 8080))  # Never attach to an existing relay.
        self.certificate = self.root / 'tls.crt'
        private = self.root / 'tls.key'
        subprocess.run(['openssl', 'req', '-x509', '-newkey', 'rsa:2048', '-nodes', '-days', '1',
                        '-subj', '/CN=bmux.dev', '-addext', 'subjectAltName=DNS:bmux.dev,DNS:live.bmux.dev',
                        '-keyout', str(private), '-out', str(self.certificate)], check=True,
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=15)
        private.chmod(0o600)
        self.log = open(self.root / 'rust-relay.log', 'wb')
        env = {name: value for name, value in os.environ.items() if name in ('PATH', 'LANG', 'TZ')}
        env.update(PUBLISHER_KEY_SHA256=hashlib.sha256(self.key.encode()).hexdigest(),
                   METRICS_KEY_SHA256=hashlib.sha256(self.metrics_key.encode()).hexdigest(),
                   AUTHORITY_PATH=str(self.root / 'authority.json'), ALLOWED_ORIGINS=ORIGIN)
        self.process = subprocess.Popen([str(Path(relay).resolve())], env=env, stdin=subprocess.DEVNULL,
                                        stdout=self.log, stderr=self.log, start_new_session=True)
        def healthy():
            assert self.process.poll() is None, 'owned relay exited'
            try:
                with urllib.request.urlopen('http://127.0.0.1:8080/health', timeout=.2) as response:
                    body = response.read(4096)
                    if body in (b'ok', b'ok\n'): return True
                    state = json.loads(body)
                    return state.get('status') == 'ok' and state.get('renderer') == 'bmwp-5'
            except OSError: return False
        wait(healthy)
        owner = self
        class Handler(http.server.BaseHTTPRequestHandler):
            def log_message(self, *_): pass
            def do_GET(self):
                if self.path.startswith('/v1/'):
                    owner.network.append({'path': self.path, 'authorization': bool(self.headers.get('Authorization'))})
                    upstream = socket.create_connection(('127.0.0.1', 8080), timeout=5)
                    try:
                        raw = f'GET {self.path} HTTP/1.1\r\n' + ''.join(f'{k}: {v}\r\n' for k, v in self.headers.items()) + '\r\n'
                        upstream.sendall(raw.encode())
                        self.connection.settimeout(10)
                        while True:
                            readable = [self.connection] if self.connection.pending() else select.select([upstream, self.connection], [], [], 10)[0]
                            if not readable: return
                            for source in readable:
                                data = source.recv(65536)
                                if not data: return
                                (self.connection if source is upstream else upstream).sendall(data)
                    except OSError: pass
                    finally: upstream.close(); self.close_connection = True
                    return
                if self.path == '/__acceptance.html':
                    data = ('<!doctype html><div id="live-broadcast" data-live-relay="https://bmux.dev/"></div><p id="live-status"></p>'
                            '<script type="module" src="/live-assets/' + owner.assets.name + '/live-page.js"></script>').encode()
                    mime = 'text/html'
                else:
                    relative = self.path.split('?', 1)[0].lstrip('/')
                    if relative.startswith('live-assets/' + owner.assets.name + '/'):
                        path = owner.assets / relative.split('/', 2)[2]
                    elif owner.site:
                        path = owner.site / relative
                        if path.is_dir(): path = path / 'index.html'
                        elif not path.is_file() and path.with_suffix('.html').is_file(): path = path.with_suffix('.html')
                    else: self.send_error(404); return
                    if not path.resolve().is_relative_to(owner.site or owner.assets) and not path.resolve().is_relative_to(owner.assets):
                        self.send_error(403); return
                    if not path.is_file(): self.send_error(404); return
                    data = path.read_bytes()
                    import mimetypes
                    mime = 'application/wasm' if path.suffix == '.wasm' else mimetypes.guess_type(path.name)[0] or 'application/octet-stream'
                self.send_response(200); self.send_header('Content-Type', mime); self.send_header('Content-Length', str(len(data)))
                self.end_headers(); self.wfile.write(data)
        self.server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
        self.server.daemon_threads = True
        context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER); context.load_cert_chain(self.certificate, private)
        self.server.socket = context.wrap_socket(self.server.socket, server_side=True)
        self.port = self.server.server_port
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True); self.thread.start()

    @property
    def chrome_rules(self):
        return f'MAP bmux.dev:443 127.0.0.1:{self.port}, MAP live.bmux.dev:443 127.0.0.1:{self.port}'

    def ws(self, publisher=False):
        headers = {'Authorization': 'Bearer ' + self.key, 'X-Bmux-Epoch': self.epoch, 'X-Bmux-Fence': self.fence} if publisher else {'Origin': ORIGIN}
        return WebSocket(self.port, self.certificate, '/v1/publisher/ws' if publisher else '/v1/viewer/ws', headers)

    def control(self, action):
        headers = {'Authorization': 'Bearer ' + self.key}
        data = b''
        if action == 'start':
            data = json.dumps({'publisher_id': str(uuid.uuid4())}).encode(); headers['Content-Type'] = 'application/json'
        else: headers.update({'X-Bmux-Epoch': self.epoch, 'X-Bmux-Fence': self.fence})
        request = urllib.request.Request('http://127.0.0.1:8080/v1/publisher/' + action, data=data, headers=headers)
        with urllib.request.urlopen(request, timeout=5) as response: state = json.load(response)
        self.epoch, self.fence = state['epoch'], state['fence']
        if action != 'heartbeat' and self.publisher: self.publisher.close(); self.publisher = None
        return state

    def publish(self, payload):
        assert payload.startswith(b'BMWP\x05') and len(payload) <= MAX_BYTES
        if self.publisher is None: self.publisher = self.ws(True)
        self.publisher.send(payload)
        response = self.publisher.receive()
        assert response.get('type') == 'ack', 'real relay did not ACK BMWP5'
        return response

    def snapshot(self):
        # Respect the relay's shared 32 HTTP/s admission budget when diagnostics
        # sample status, frames and metadata in one native assertion iteration.
        time.sleep(max(0, self.last_snapshot + .075 - time.monotonic()))
        self.last_snapshot = time.monotonic()
        peer = self.ws()
        try:
            state = peer.receive(); assert state['type'] == 'state'
            if state['state'] != 'live': return state, None
            payload = peer.receive()
            while not isinstance(payload, bytes):
                assert payload.get('type') == 'state' and payload['epoch'] == state['epoch']
                if payload['state'] != 'live': return payload, None
                payload = peer.receive()
            assert payload.startswith(b'BMWP\x05')
            return state, payload
        finally: peer.close()

    def metrics(self):
        request = urllib.request.Request('http://127.0.0.1:8080/metrics', headers={'Authorization': 'Bearer ' + self.metrics_key})
        with urllib.request.urlopen(request, timeout=3) as response:
            text = response.read(4096).decode()
        return {line.split()[0]: int(line.split()[1]) for line in text.splitlines() if line and not line.startswith('#')}

    def close(self):
        if self.publisher: self.publisher.close()
        if self.server:
            if hasattr(self, 'thread'): self.server.shutdown()
            self.server.server_close()
        if self.process and self.process.poll() is None:
            self.process.terminate()
            try: self.process.wait(timeout=5)
            except subprocess.TimeoutExpired: self.process.kill(); self.process.wait(timeout=5)
        if self.log: self.log.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--relay', required=True, type=Path)
    parser.add_argument('--assets', required=True, type=Path)
    parser.add_argument('--frame', required=True, type=Path)
    args = parser.parse_args()
    payload = args.frame.read_bytes(); assert payload.startswith(b'BMWP\x05')
    with tempfile.TemporaryDirectory(prefix='bmux-rust-relay-smoke-') as directory:
        stack = RelayStack(args.relay, Path(directory), args.assets)
        try:
            stack.control('start'); ack = stack.publish(payload)
            state, received = stack.snapshot(); assert state['state'] == 'live' and received == payload
            stack.control('pause'); assert stack.snapshot()[1] is None
            stack.control('resume'); stack.publish(payload); assert stack.snapshot()[1] == payload
            stack.control('stop'); assert stack.snapshot()[0]['state'] == 'offline'
            print(json.dumps({'result': 'PASS', 'protocol': 'BMWP5/WSS', 'revision': ack['revision'],
                              'relay_sha256': hashlib.sha256(args.relay.read_bytes()).hexdigest(),
                              'cases': ['rust-relay-real-frame', 'pause-clears', 'resume-full', 'stop-offline']}))
        finally: stack.close()


if __name__ == '__main__': main()
