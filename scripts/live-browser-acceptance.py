#!/usr/bin/env python3
"""Real generated-site/WASM/relay browser regression; stdlib, isolated owned children."""
import argparse
import hashlib
import http.client
import http.server
import importlib.util
import json
import mimetypes
import os
from pathlib import Path
import secrets
import subprocess
import tempfile
import threading
import time
import urllib.request
import uuid

ROOT = Path(__file__).resolve().parent.parent
spec = importlib.util.spec_from_file_location('cdp', ROOT / 'scripts/web-browser-cdp.py')
cdp_module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(cdp_module)


def digest(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def wait(predicate, seconds=10):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        result = predicate()
        if result:
            return result
        time.sleep(0.1)
    raise AssertionError('bounded browser acceptance condition timed out')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--browser', required=True, type=Path)
    parser.add_argument('--site', required=True, type=Path)
    parser.add_argument('--fixture', required=True, type=Path)
    args = parser.parse_args()
    site = args.site.resolve()
    version = (site / 'live-assets/current-version').read_text().strip()
    assets = site / 'live-assets' / version
    frame = args.fixture.read_bytes()
    assert frame.startswith(b'BMWP\x03')
    module_path = f'/live-assets/{version}/live-page.js'
    generated_pages = list(site.rglob('*.html'))
    assert generated_pages
    for page in generated_pages:
        if not page.is_relative_to(site / 'live-assets') and not page.is_relative_to(site / 'public'):
            assert module_path in page.read_text(), f'missing immutable audience hook in {page.name}'
    evidence = {'browser': str(args.browser), 'browser_sha256': digest(args.browser.resolve()),
                'wasm_sha256': digest(assets / 'bmux_web_renderer_bg.wasm'),
                'fixture_sha256': digest(args.fixture), 'bundle': version, 'cases': []}
    children = []
    browser = None
    proxy = None
    cdp = None
    with tempfile.TemporaryDirectory(prefix='bmux-live-browser-') as temporary:
        isolated = Path(temporary)
        key = secrets.token_hex(32)
        keyfile = isolated / 'key'
        keyfile.write_text(key)
        keyfile.chmod(0o600)
        log = open(isolated / 'children.log', 'wb')
        try:
            relay = subprocess.Popen(['node', str(ROOT / 'scripts/live-dev.mjs'), '--assets', str(assets),
                '--key-file', str(keyfile), '--state-file', str(isolated / 'state'),
                '--ready-file', str(isolated / 'ready')], stdout=log, stderr=log)
            children.append(relay)
            ready = wait(lambda: (isolated / 'ready').exists())
            relay_url = json.loads((isolated / 'ready').read_text())['url']
            relay_port = int(relay_url.rsplit(':', 1)[1])
            network = []

            class Handler(http.server.BaseHTTPRequestHandler):
                def log_message(self, *_args):
                    pass

                def do_GET(self):
                    if self.path.startswith('/api/live/'):
                        network.append({'path': self.path.split('?')[0],
                                        'authorization': bool(self.headers.get('Authorization')),
                                        'controller': bool(self.headers.get('X-Bmux-Controller'))})
                        upstream = http.client.HTTPConnection('127.0.0.1', relay_port, timeout=6)
                        try:
                            upstream.request('GET', self.path)
                            response = upstream.getresponse()
                            data = response.read(8 * 1024 * 1024 + 1)
                            self.send_response(response.status)
                            for name in ('Content-Type', 'X-Bmux-Epoch', 'X-Snapshot-Revision', 'Cache-Control'):
                                if response.getheader(name):
                                    self.send_header(name, response.getheader(name))
                            self.send_header('Content-Length', str(len(data)))
                            self.end_headers()
                            self.wfile.write(data)
                        finally:
                            upstream.close()
                        return
                    relative = self.path.split('?')[0].lstrip('/')
                    if relative in ('', 'live', 'live/'):
                        relative = 'live.html' if relative else 'index.html'
                    path = (site / relative).resolve()
                    if not path.is_relative_to(site) or not path.is_file():
                        self.send_error(404)
                        return
                    data = path.read_bytes()
                    self.send_response(200)
                    self.send_header('Content-Type', 'application/wasm' if path.suffix == '.wasm'
                                     else mimetypes.guess_type(path)[0] or 'application/octet-stream')
                    self.send_header('Content-Length', str(len(data)))
                    self.end_headers()
                    self.wfile.write(data)

            proxy = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
            proxy.daemon_threads = True
            thread = threading.Thread(target=proxy.serve_forever, daemon=True)
            thread.start()
            page_url = f'http://127.0.0.1:{proxy.server_port}/live'
            evidence['html_mode'] = 'generated-site'
            browser = subprocess.Popen([str(args.browser), '--headless', '--no-sandbox', '--disable-gpu',
                '--disable-background-networking', '--no-first-run', '--no-default-browser-check',
                '--remote-debugging-port=0', '--remote-allow-origins=*',
                f'--user-data-dir={isolated / "profile"}', 'about:blank'], stdout=log, stderr=log)
            children.append(browser)
            active = isolated / 'profile/DevToolsActivePort'
            wait(active.exists)
            port = active.read_text().splitlines()[0]
            with urllib.request.urlopen(f'http://127.0.0.1:{port}/json/list', timeout=5) as response:
                targets = json.load(response)
            cdp = cdp_module.CDP(next(t['webSocketDebuggerUrl'] for t in targets if t['type'] == 'page'))
            cdp.call('Page.enable')
            cdp.call('Page.addScriptToEvaluateOnNewDocument', {'source': '''
              window.__proof = {texts:[], images:0};
              const text = CanvasRenderingContext2D.prototype.fillText;
              CanvasRenderingContext2D.prototype.fillText = function(value,...args) {
                __proof.texts.push(String(value)); return text.call(this,value,...args);
              };
              const image = CanvasRenderingContext2D.prototype.drawImage;
              CanvasRenderingContext2D.prototype.drawImage = function(...args) {
                __proof.images++; return image.apply(this,args);
              };
            '''})
            cdp.call('Page.navigate', {'url': page_url})
            wait(lambda: cdp.evaluate("!!document.getElementById('live-broadcast')"))
            epoch = None

            def publish(action, data=None, revision=None):
                headers = {'Authorization': f'Bearer {key}'}
                if epoch:
                    headers['X-Bmux-Epoch'] = epoch
                if revision:
                    headers['X-Bmux-Revision'] = str(revision)
                if isinstance(data, dict):
                    headers['Content-Type'] = 'application/json'
                    data = json.dumps(data).encode()
                elif data is not None:
                    headers['Content-Type'] = 'application/octet-stream'
                request = urllib.request.Request(relay_url + '/api/live/main/publisher/' + action,
                    data=data if data is not None else b'', headers=headers,
                    method='PUT' if action == 'frame' else 'POST')
                with urllib.request.urlopen(request, timeout=6) as response:
                    assert response.status in (200, 204)
                    return json.load(response) if response.status == 200 else None

            def start():
                return publish('start', {'protocol': 1, 'renderer': 'bmwp-3',
                                        'publisher_id': str(uuid.uuid4())})['epoch']

            def visible():
                return cdp.evaluate("!!document.querySelector('#live-broadcast canvas:not([hidden])')")

            def hidden():
                return not visible() and cdp.evaluate("document.querySelectorAll('#live-broadcast canvas').length === 0")

            def pixels():
                return cdp.evaluate('''(() => {
                  const c=document.querySelector('#live-broadcast canvas:not([hidden])');
                  const x=c.getContext('2d'); const sample=(a,b)=>Array.from(x.getImageData(a,b,1,1).data);
                  const d=x.getImageData(20,20,700,20).data;
                  let bright=0; for(let i=0;i<d.length;i+=4) if(d[i]>80||d[i+1]>80||d[i+2]>80) bright++;
                  return {width:c.width,height:c.height,brightTitlePixels:bright,
                    red:sample(30,150),green:sample(90,150),blue:sample(30,210),
                    imageDrawCalls:__proof.images,text:__proof.texts.join('')};
                })()''')

            epoch = start()
            publish('frame', frame, 1)
            try:
                wait(visible)
            except AssertionError:
                print(json.dumps({'diagnostic': cdp.evaluate("({status:document.getElementById('live-status')?.textContent, scripts:Array.from(document.scripts).map(s=>s.src), resources:performance.getEntriesByType('resource').map(r=>({url:r.name,status:r.responseStatus}))})"), 'audience_requests': len(network)}))
                raise
            proof = pixels()
            assert proof['width'] == 800 and proof['height'] == 480
            assert proof['brightTitlePixels'] > 30
            assert 'bmux_tui' in proof['text'] and 'browser' in proof['text']
            assert proof['imageDrawCalls'] > 0
            assert proof['red'][0] > 220 and proof['green'][1] > 220 and proof['blue'][2] > 220
            evidence['canvas'] = proof
            evidence['cases'].append('generated-live-real-wasm-text-image-pixels')
            publish('pause'); wait(hidden)
            time.sleep(1.2); assert hidden()
            evidence['cases'].append('pause-clears')
            publish('resume'); publish('frame', frame, 2); wait(visible)
            evidence['cases'].append('resume-identical-static-source-frame')
            publish('stop'); wait(hidden)
            evidence['cases'].append('stop-clears')
            epoch = start(); publish('frame', frame, 1); wait(visible)
            evidence['cases'].append('replacement-epoch-reconnect')
            wait(hidden, 18)
            evidence['cases'].append('actual-relay-lease-expiry-clears')
            epoch = start(); publish('frame', frame, 1); wait(visible)
            evidence['cases'].append('reconnect-after-expiry')
            cdp.call('Page.navigate', {'url': f'http://127.0.0.1:{proxy.server_port}/'})
            wait(lambda: cdp.evaluate("document.readyState === 'complete' && !document.getElementById('live-broadcast')"))
            time.sleep(1.2)
            count = len(network); time.sleep(1.3); assert len(network) == count
            evidence['cases'].append('navigation-unmount-stops-polling')
            assert network and all(not n['authorization'] and not n['controller'] for n in network)
            assert all(n['path'] in ('/api/live/main/status', '/api/live/main/presentation') for n in network)
            evidence['audience_requests'] = len(network)
            evidence['cases'].append('network-output-only-no-controller-credentials')
            evidence['status'] = 'PASS'
        finally:
            if cdp:
                cdp.close()
            for child in reversed(children):
                child.terminate()
                try:
                    child.wait(timeout=8)
                except subprocess.TimeoutExpired:
                    child.kill(); child.wait(timeout=5)
            if proxy:
                proxy.shutdown(); proxy.server_close()
            log.close()
            evidence['cleanup'] = 'owned children reaped; proxy closed; private profile/state removed'
    print(json.dumps(evidence, ensure_ascii=False))


if __name__ == '__main__':
    main()
