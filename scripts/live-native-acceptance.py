#!/usr/bin/env python3
"""Actual native controller/presenter/terminal/relay acceptance; no builds or user state.

Requires an explicit verified test executable and staged generated WASM assets.
Owns only fresh temporary paths and child process groups. Never prints child logs,
publisher credentials, proofs, deck contents, or response bodies on failure.
"""
import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import secrets
import signal
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
import uuid

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parent.parent


def load(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


PRESENTER = load('native_presenter_helpers', ROOT / 'scripts/presenter-web-acceptance.py')
CDP = load('native_cdp_helpers', ROOT / 'scripts/web-browser-cdp.py')


def digest(path):
    with open(path, 'rb') as handle:
        return hashlib.file_digest(handle, 'sha256').hexdigest()


def require(condition, label):
    if not condition:
        raise AssertionError(label)


def wait(label, fn, seconds=25):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        value = fn()
        if value:
            return value
        time.sleep(.15)
    raise AssertionError(label + ' timed out')


class Harness:
    def __init__(self, args, root):
        self.args, self.root = args, root
        self.children, self.terminals, self.logs = [], [], []
        self.env = {k: v for k, v in os.environ.items() if k in ('PATH', 'LANG', 'LC_ALL', 'TZ')}
        for name in ('home', 'config', 'runtime', 'data', 'state', 'logs', 'cache', 'tmp'):
            (root / name).mkdir(mode=0o700)
        for key, name in [('HOME', 'home'), ('TMPDIR', 'tmp'), ('XDG_CONFIG_HOME', 'config'),
                          ('XDG_RUNTIME_DIR', 'runtime'), ('XDG_DATA_HOME', 'data'),
                          ('XDG_STATE_HOME', 'state'), ('XDG_CACHE_HOME', 'cache'),
                          ('BMUX_CONFIG_DIR', 'config'), ('BMUX_RUNTIME_DIR', 'runtime'),
                          ('BMUX_DATA_DIR', 'data'), ('BMUX_STATE_DIR', 'state'), ('BMUX_LOG_DIR', 'logs')]:
            self.env[key] = str(root / name)
        self.config = root / 'config/bmux.toml'
        self.env.update(BMUX_CONFIG=str(self.config), BMUX_NO_BASE_CONFIG='1',
                        BMUX_RUNTIME_NAME='default', BMUX_TARGET='local', TERM='xterm-256color')
        producer = root / 'terminal-producer'
        producer.write_text('#!' + sys.executable + '\nimport time\nprint("NATIVE_TERMINAL_PUBLIC_5831", flush=True)\ntime.sleep(180)\n')
        producer.chmod(0o700)
        self.config.write_text('[general]\ndefault_shell = ' + json.dumps(str(producer)) + '\n'
            '[plugins]\nenabled = ["bmux.sessions", "bmux.contexts", "bmux.clients", '
            '"bmux.permissions", "bmux.pane_runtime", "bmux.tabs", "bmux.playbook", '
            '"bmux.presenter", "bmux.broadcast"]\n[presenter.web]\nmode = "off"\n')
        self.command = [str(args.binary), '--config', str(self.config)]
        self.key = root / 'publisher-key'
        self.key.write_text(secrets.token_urlsafe(48))
        self.key.chmod(0o600)
        self.cdp = None
        self.cases = []

    def run(self, *args, check=True):
        result = subprocess.run(self.command + list(args), env=self.env, stdin=subprocess.DEVNULL,
                                capture_output=True, timeout=15)
        require(not check or result.returncode == 0, 'native command rejected: ' + args[0])
        return result

    def spawn(self, argv):
        log = open(self.root / ('child-' + str(len(self.children)) + '.log'), 'wb')
        self.logs.append(log)
        child = subprocess.Popen(argv, env=self.env, stdin=subprocess.DEVNULL, stdout=log, stderr=log,
                                 start_new_session=True)
        self.children.append(child)
        return child

    def http(self, path):
        try:
            with urllib.request.urlopen(self.url + '/api/live/main/' + path, timeout=4) as response:
                body = response.read(8 * 1024 * 1024 + 1)
                require(len(body) <= 8 * 1024 * 1024, 'oversized audience response')
                return response.status, body, dict(response.headers)
        except urllib.error.HTTPError as error:
            return error.code, error.read(4096), dict(error.headers)

    def status(self):
        code, body, _ = self.http('status')
        require(code == 200, 'relay status unavailable')
        return json.loads(body)

    def control(self, *args):
        return json.loads(self.run('broadcast', *args).stdout)

    def live(self):
        return self.status()['state'] == 'live' and self.control('status')['local'] == 'running'

    def frame(self):
        code, body, headers = self.http('presentation?epoch=' + str(self.status()['epoch']))
        require(code == 200 and body.startswith(b'BMWP\x02'), 'complete BMWP2 audience frame missing')
        return body, headers

    def inspect(self, expected, absent):
        # Exact HTTP-delivered bytes are fetched and decoded/presented by the real
        # generated Rust/WASM renderer, including complete metadata and images.
        self.stage = 'WASM-frame-inspection'
        expression = '''(async()=>{
          window.__nativeText=[];
          const response=await fetch('/api/live/main/presentation?epoch='+encodeURIComponent((await (await fetch('/api/live/main/status')).json()).epoch));
          if(response.status!==200)return {ready:false};
          const bytes=new Uint8Array(await response.arrayBuffer());
          nativeRenderer.reset();
          const info=JSON.parse(nativeRenderer.inspect_binary(bytes));
          await nativeRenderer.present_binary(bytes);
          return {info, text:__nativeText.join(''), metadata:JSON.parse(nativeRenderer.metadata_json())};
        })()'''
        result = self.cdp.evaluate(expression)
        require(result.get('ready', True), 'audience not yet ready')
        require(result['info']['supported'], 'generated WASM rejects native metadata support')
        require(expected in result['text'], 'real WASM rendered audience text missing')
        encoded = json.dumps(result)
        require(all(secret not in encoded for secret in absent), 'private/unrevealed content leaked')
        return result

    def browser(self):
        self.stage = 'browser-launch'
        child = self.spawn([str(self.args.browser), '--headless', '--no-sandbox', '--disable-gpu',
            '--disable-background-networking', '--no-first-run', '--no-default-browser-check',
            '--remote-debugging-port=0', '--remote-allow-origins=*',
            '--user-data-dir=' + str(self.root / 'browser-profile'), 'about:blank'])
        port_file = self.root / 'browser-profile/DevToolsActivePort'
        wait('isolated browser', lambda: port_file.exists() and child.poll() is None)
        port = port_file.read_text().splitlines()[0]
        with urllib.request.urlopen('http://127.0.0.1:' + port + '/json/list', timeout=5) as response:
            targets = json.load(response)
        self.cdp = CDP.CDP(next(t['webSocketDebuggerUrl'] for t in targets if t['type'] == 'page'))
        self.cdp.call('Page.enable')
        self.cdp.call('Page.navigate', {'url': self.url + '/live/'})
        wait('same-origin live document', lambda: self.cdp.evaluate("!!document.getElementById('live-broadcast')"))
        # Let the page own initial WASM initialization before creating the separate
        # diagnostic renderer; concurrent wasm-bindgen init can race module state.
        wait('live page initial Canvas commit', lambda: self.cdp.evaluate("Array.from(document.querySelectorAll('#live-broadcast canvas')).some(c=>!c.hidden)"))
        self.stage = 'WASM-module-initialization'
        self.cdp.evaluate('''(async()=>{
          const m=await import(''' + json.dumps(self.asset_url + 'bmux_web_renderer.js') + ''');
          await m.default();
          const canvas=document.createElement('canvas');canvas.id='native-proof';document.body.append(canvas);
          window.__nativeText=[];
          const original=CanvasRenderingContext2D.prototype.fillText;
          CanvasRenderingContext2D.prototype.fillText=function(text,...args){__nativeText.push(String(text));return original.call(this,text,...args)};
          window.nativeRenderer=new m.CanvasRenderer('native-proof',8,16);
        })()''')

    def execute(self):
        version = self.run('--version').stdout.decode().strip()
        require(version == 'bmux 0.0.1-alpha.1', 'test executable version mismatch')
        self.spawn(self.command + ['server', 'start', '--foreground-internal', '--no-pane-shell-integration'])
        def daemon_ready():
            result = self.run('server', 'status', '--json', check=False)
            return result.returncode == 0 and json.loads(result.stdout).get('running')
        wait('fresh compatible foreground daemon', daemon_ready)
        self.cases.append('verified-binary-fresh-daemon-compatible-client-isolated-paths')
        ready = self.root / 'relay-ready.json'
        self.spawn(['node', str(ROOT / 'scripts/live-dev.mjs'), '--assets', str(self.args.assets),
                    '--key-file', str(self.key), '--state-file', str(self.root / 'relay.json'),
                    '--ready-file', str(ready)])
        wait('isolated relay', ready.exists)
        self.url = json.loads(ready.read_text())['url']
        self.asset_url = '/live-assets/' + self.args.assets.name + '/'
        source = str(uuid.uuid4())
        deck = self.root / 'audience.md'
        public, private, hidden = 'NATIVE_AUDIENCE_PUBLIC_5831', 'NATIVE_SPEAKER_PRIVATE_5831', 'NATIVE_UNREVEALED_5831'
        deck.write_text('# ' + public + '\n\n<!-- bmux:notes -->\n' + private +
                        '\n<!-- bmux:end-notes -->\n\n<!-- bmux:pause -->\n' + hidden + '\n')
        presenter = PRESENTER.Mirror(self.command + ['present', 'play', str(deck), '--presenter', '--broadcast', source], self.env)
        self.terminals.append(presenter)
        wait('local speaker remains usable', lambda: public.encode() in presenter.bytes() and private.encode() in presenter.bytes())
        wait('presenter source registered before controller', lambda: b': registered;' in presenter.bytes())
        require(b'Local browser URL' not in presenter.bytes() and b'Read-only audience browser:' not in presenter.bytes(),
                'broadcast-only activated HTTP listener')
        self.control('start', self.url, 'main', str(self.key), source)
        wait('presenter controller live', self.live)
        first, _ = self.frame()
        self.browser()
        self.inspect(public, [private, hidden])
        try:
            wait('actual live page visible', lambda: self.cdp.evaluate("Array.from(document.querySelectorAll('#live-broadcast canvas')).some(c=>!c.hidden)"))
        except AssertionError:
            state = self.cdp.evaluate("({status:document.getElementById('live-status').textContent,canvases:document.querySelectorAll('#live-broadcast canvas').length})")
            allowed = {'Offline', 'Live · read-only', 'Broadcast viewer unavailable', 'Broadcast unavailable or incompatible'}
            self.viewer_diagnostic = {'status': state['status'] if state['status'] in allowed else 'other', 'canvases': state['canvases']}
            raise
        require(not (self.root / 'state/cache').exists(), 'broadcast-only downloaded local web assets')
        self.cases.append('real-presenter-no-local-web-full-inline-WASM-audience-private-notes-hidden-excluded')
        before_pause = self.status()
        self.control('pause')
        wait('pause closes audience gate', lambda: self.status()['state'] == 'paused')
        require(self.http('presentation?epoch=' + str(self.status()['epoch']))[0] in (409,503), 'paused relay exposes content')
        wait('actual live page pause clears Canvas', lambda: self.cdp.evaluate("!Array.from(document.querySelectorAll('#live-broadcast canvas')).some(c=>!c.hidden)"))
        require(presenter.child.poll() is None, 'pause broke local playback')
        self.control('resume')
        wait('unchanged slide refresh republished', self.live)
        self.frame()
        require(int(self.status()['revision']) > int(before_pause['revision']), 'resume did not publish a fresh relay revision')
        self.inspect(public, [private, hidden])
        self.cases.append('pause-clear-resume-unchanged-slide-refresh')
        self.run('new-session', 'native-acceptance')
        sessions = json.loads(self.run('list-sessions', '--json').stdout)
        if isinstance(sessions, dict): sessions = sessions['sessions']
        require(len(sessions) == 1, 'unexpected session outside owned runtime')
        terminal_source = str(uuid.uuid4())
        self.control('terminal', terminal_source, sessions[0]['id'], str(self.args.binary), self.args.sha256,
                     version, '80', '24')
        self.control('source', terminal_source)
        wait('terminal controller live', self.live)
        wait('terminal observer audience', lambda: self.inspect_terminal())
        self.frame()
        for _ in range(4):
            time.sleep(.6)
            current, _ = self.frame()
            require(current != first, 'old presenter raced source switch')
            self.inspect('NATIVE_TERMINAL_PUBLIC_5831', [public, private, hidden])
        require(presenter.child.poll() is None, 'switch broke local presenter')
        self.cases.append('terminal-observer-switch-old-presenter-fenced-local-playback-survives')
        self.control('source', source)
        wait('presenter source switched back', self.live)
        wait('presenter repaired audience', lambda: self.inspect_presenter(public, private, hidden))
        self.control('stop')
        wait('controller stop offline', lambda: self.status()['state'] == 'offline')
        require(self.http('presentation?epoch=' + str(self.status()['epoch']))[0] in (409,503), 'stopped relay exposes content')
        wait('actual live page stop clears Canvas', lambda: self.cdp.evaluate("!Array.from(document.querySelectorAll('#live-broadcast canvas')).some(c=>!c.hidden)"))
        require(presenter.child.poll() is None, 'stop broke local playback')
        presenter.send(b'q')
        wait('local presenter graceful quit', lambda: presenter.child.poll() is not None)
        require(presenter.child.returncode == 0, 'presenter graceful quit failed')
        self.cases.append('source-switch-full-repair-stop-offline-local-quit')
        self.run('server', 'stop')
        secret = self.key.read_bytes().strip()
        for log in self.logs:
            log.flush()
        require(all(secret not in path.read_bytes() for path in self.root.glob('child-*.log')), 'publisher key leaked into child logs')
        require(secret not in presenter.bytes(), 'publisher key leaked into presenter terminal')
        return {'binary': str(self.args.binary), 'sha256': self.args.sha256, 'version': version,
                'wasm_sha256': digest(self.args.assets / 'bmux_web_renderer_bg.wasm'), 'cases': self.cases}

    def inspect_terminal(self):
        try:
            self.inspect('NATIVE_TERMINAL_PUBLIC_5831', ['NATIVE_AUDIENCE_PUBLIC_5831', 'NATIVE_SPEAKER_PRIVATE_5831'])
            return True
        except AssertionError:
            return False

    def inspect_presenter(self, public, private, hidden):
        try:
            self.inspect(public, [private, hidden])
            return True
        except AssertionError:
            return False

    def close(self):
        if self.cdp:
            self.cdp.close()
        for terminal in reversed(self.terminals):
            terminal.stop()
        for child in reversed(self.children):
            if child.poll() is None:
                os.killpg(child.pid, signal.SIGTERM)
                try:
                    child.wait(timeout=8)
                except subprocess.TimeoutExpired:
                    os.killpg(child.pid, signal.SIGKILL)
                    child.wait(timeout=5)
        for log in self.logs:
            log.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', required=True, type=Path)
    parser.add_argument('--sha256', required=True)
    parser.add_argument('--assets', required=True, type=Path)
    parser.add_argument('--browser', required=True, type=Path)
    args = parser.parse_args()
    args.binary, args.assets, args.browser = args.binary.resolve(), args.assets.resolve(), args.browser.resolve()
    require(digest(args.binary) == args.sha256, 'explicit test binary SHA256 mismatch')
    with tempfile.TemporaryDirectory(prefix='bmux-live-native-') as directory:
        root = Path(directory)
        harness = Harness(args, root)
        try:
            report = harness.execute()
            print(json.dumps({'result': 'PASS', **report}, sort_keys=True))
        except Exception as error:
            print(json.dumps({'result': 'FAIL', 'error_type': type(error).__name__,
                              'stage': str(error) if isinstance(error, AssertionError) else getattr(harness, 'stage', 'owned-stack-operation'),
                              'passed_cases': harness.cases, 'viewer_diagnostic': getattr(harness, 'viewer_diagnostic', None)}, sort_keys=True))
            return 1
        finally:
            harness.close()
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
