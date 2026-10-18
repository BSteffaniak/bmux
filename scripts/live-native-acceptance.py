#!/usr/bin/env python3
"""Actual native controller/presenter/terminal/relay acceptance; no builds or user state.

Requires an explicit verified test executable and staged generated WASM assets.
Owns only fresh temporary paths and child process groups. Never prints child logs,
publisher credentials, proofs, deck contents, or response bodies on failure.
"""
import argparse
import base64
import fcntl
import struct
import termios
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
import tomllib
import traceback
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
LOCAL = load('native_relay_helpers', ROOT / 'scripts/smoke-live-local.py')


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


def browser_ready(child, port_file):
    code = child.poll()
    if code is not None:
        raise AssertionError(f'isolated browser exited before readiness (code {code})')
    return port_file.exists()


def browser_environment(env, directory):
    # Chromium creates a Unix singleton socket beneath TMPDIR. Runner/Nix temp
    # prefixes can exceed sockaddr_un's path limit; use an owned short directory,
    # never the user's browser profile or ambient temporary directory.
    return dict(env, TMPDIR=str(directory))


class Harness:
    def __init__(self, args, root):
        self.args, self.root = args, root
        self.children, self.terminals, self.logs = [], [], []
        self.browser_temp = None
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
        self.producer_mode = root / 'producer-mode'
        self.producer_mode.write_text('idle')
        producer.write_text('#!' + sys.executable + '\nimport time, pathlib\n'
            + 'modefile=pathlib.Path(' + repr(str(self.producer_mode)) + ')\n'
            + 'print("NATIVE_TERMINAL_PUBLIC_5831", flush=True)\nrevision=0\n'
            + 'while True:\n'
            + ' mode=modefile.read_text()\n'
            + ' if mode=="timer":\n'
            + '  print("\\rTIMER "+str(revision%10), end="", flush=True); revision+=1\n'
            + ' elif mode=="stream":\n'
            + '  print("NATIVE_TERMINAL_PUBLIC_5831", revision, flush=True); revision+=1\n'
            + ' time.sleep(.2)\n')
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
        self.stack = None
        self.cases = []

    def run(self, *args, check=True):
        result = subprocess.run(self.command + list(args), env=self.env, stdin=subprocess.DEVNULL,
                                capture_output=True, timeout=15)
        require(not check or result.returncode == 0, 'native command rejected: ' + args[0])
        return result

    def spawn(self, argv, *, env=None):
        log = open(self.root / ('child-' + str(len(self.children)) + '.log'), 'wb')
        self.logs.append(log)
        child = subprocess.Popen(argv, env=self.env if env is None else env, stdin=subprocess.DEVNULL, stdout=log, stderr=log,
                                 start_new_session=True)
        self.children.append(child)
        return child

    def status(self):
        state, payload = self.stack.snapshot()
        state['revision'] = '0'
        if payload:
            result = subprocess.run([str(self.args.wire_tool), '--inspect'], input=payload,
                                    capture_output=True, check=True, timeout=5)
            state['revision'] = json.loads(result.stdout)['revision']
        return state

    def control(self, *args):
        return json.loads(self.run('broadcast', *args).stdout)

    def live(self):
        return self.status()['state'] == 'live' and self.control('status')['local'] == 'running'

    def frame(self):
        state, body = self.stack.snapshot()
        require(state['state'] == 'live' and body and body.startswith(b'BMWP\x04'), 'complete BMWP4 audience frame missing')
        return body, {}

    def inspect(self, expected, absent):
        # Decode an actual full repair delivered by viewer WSS, never a fabricated
        # scene or an obsolete HTTP adapter response.
        self.stage = 'WASM-frame-inspection'
        payload, _ = self.frame()
        expression = '''(async()=>{
          window.__nativeText=[];
          const bytes=Uint8Array.from(atob(''' + json.dumps(base64.b64encode(payload).decode()) + '''),c=>c.charCodeAt(0));
          nativeRenderer.reset();
          const info=JSON.parse(nativeRenderer.inspect_binary(bytes));
          await nativeRenderer.present_binary(bytes);
          return {info,text:__nativeText.join(''),metadata:JSON.parse(nativeRenderer.metadata_json())};
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
        self.browser_temp = tempfile.TemporaryDirectory(prefix='bmux-cdp-', dir='/tmp')
        child = self.spawn([str(self.args.browser), '--headless', '--no-sandbox', '--disable-gpu',
            '--disable-background-networking', '--no-first-run', '--no-default-browser-check',
            '--no-proxy-server', '--ignore-certificate-errors', '--host-resolver-rules=' + self.stack.chrome_rules,
            '--remote-debugging-port=0', '--remote-allow-origins=*',
            '--user-data-dir=' + str(self.root / 'browser-profile'), 'about:blank'],
            env=browser_environment(self.env, self.browser_temp.name))
        port_file = self.root / 'browser-profile/DevToolsActivePort'
        wait('isolated browser', lambda: browser_ready(child, port_file))
        port = port_file.read_text().splitlines()[0]
        with urllib.request.urlopen('http://127.0.0.1:' + port + '/json/list', timeout=5) as response:
            targets = json.load(response)
        self.cdp = CDP.CDP(next(t['webSocketDebuggerUrl'] for t in targets if t['type'] == 'page'))
        self.cdp.call('Page.enable')
        self.cdp.call('Page.addScriptToEvaluateOnNewDocument', {'source': '''
          window.__wire=[];const Original=WebSocket;window.WebSocket=class extends Original{
          constructor(...args){super(...args);this.addEventListener('message',event=>{
          if(event.data instanceof ArrayBuffer)__wire.push({bytes:event.data.byteLength,time:performance.now()})})}};
        '''})
        self.cdp.call('Page.navigate', {'url': 'https://bmux.dev/__acceptance.html'})
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
        expected_version = 'bmux ' + tomllib.loads((ROOT / 'Cargo.toml').read_text())['workspace']['package']['version']
        require(version == expected_version, 'test executable version mismatch')
        self.spawn(self.command + ['server', 'start', '--foreground-internal', '--no-pane-shell-integration'])
        def daemon_ready():
            result = self.run('server', 'status', '--json', check=False)
            return result.returncode == 0 and json.loads(result.stdout).get('running')
        wait('fresh compatible foreground daemon', daemon_ready)
        self.cases.append('verified-binary-fresh-daemon-compatible-client-isolated-paths')
        self.stack = LOCAL.RelayStack(self.args.relay, self.root, self.args.assets)
        # Local native control uses loopback HTTP; the real browser and snapshot
        # clients use the temporary TLS/WSS edge. No production state is touched.
        self.url = 'http://127.0.0.1:8080'
        self.key.write_text(self.stack.key)
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
        require(self.stack.snapshot()[1] is None, 'paused relay exposes content')
        wait('actual live page pause clears Canvas', lambda: self.cdp.evaluate("!Array.from(document.querySelectorAll('#live-broadcast canvas')).some(c=>!c.hidden)"))
        require(presenter.child.poll() is None, 'pause broke local playback')
        self.control('resume')
        wait('unchanged slide refresh republished', self.live)
        self.frame()
        require(self.status()['epoch'] == before_pause['epoch'] and self.status()['fence'] != before_pause['fence'], 'resume did not fence the previous stream')
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
        self.timer_and_idle()
        self.producer_mode.write_text('stream')
        for _ in range(4):
            time.sleep(.6)
            current, _ = self.frame()
            require(current != first, 'old presenter raced source switch')
            self.inspect('NATIVE_TERMINAL_PUBLIC_5831', [public, private, hidden])
        require(presenter.child.poll() is None, 'switch broke local presenter')
        self.cases.append('terminal-observer-switch-old-presenter-fenced-local-playback-survives')
        self.verify_observed_resizing(sessions[0]['id'])
        self.control('source', source)
        wait('presenter source switched back', self.live)
        wait('presenter repaired audience', lambda: self.inspect_presenter(public, private, hidden))
        self.control('stop')
        wait('controller stop offline', lambda: self.status()['state'] == 'offline')
        require(self.stack.snapshot()[1] is None, 'stopped relay exposes content')
        wait('actual live page stop clears Canvas', lambda: self.cdp.evaluate("!Array.from(document.querySelectorAll('#live-broadcast canvas')).some(c=>!c.hidden)"))
        require(presenter.child.poll() is None, 'stop broke local playback')
        presenter.send(b'q')
        wait('local presenter graceful quit', lambda: presenter.child.poll() is not None)
        require(presenter.child.returncode == 0, 'presenter graceful quit failed')
        self.cases.append('source-switch-full-repair-stop-offline-local-quit')
        self.turnkey_profiles(deck, public, private, hidden)
        self.run('server', 'stop')
        secret = self.key.read_bytes().strip()
        for log in self.logs:
            log.flush()
        require(all(secret not in path.read_bytes() for path in self.root.glob('child-*.log')), 'publisher key leaked into child logs')
        require(secret not in presenter.bytes(), 'publisher key leaked into presenter terminal')
        return {'binary': str(self.args.binary), 'sha256': self.args.sha256, 'version': version,
                'relay_sha256': digest(self.args.relay), 'wire_tool_sha256': digest(self.args.wire_tool),
                'wasm_sha256': digest(self.args.assets / 'bmux_web_renderer_bg.wasm'), 'cases': self.cases}

    def turnkey_profiles(self, deck, public, private, hidden):
        self.run('broadcast', 'setup', '--profile', 'acceptance', '--endpoint', self.url,
                 '--channel', 'main', '--credential-file', str(self.key), '--default')

        def launch(*flags):
            mirror = PRESENTER.Mirror(self.command + ['present', 'play', str(deck), '--presenter', *flags], self.env)
            self.terminals.append(mirror)
            wait('turnkey local speaker remains usable',
                 lambda: public.encode() in mirror.bytes() and private.encode() in mirror.bytes())
            return mirror

        def quit_presenter(mirror):
            mirror.send(b'q')
            wait('turnkey presenter graceful quit', lambda: mirror.child.poll() is not None)
            require(mirror.child.returncode == 0, 'turnkey presenter quit failed')

        automatic = launch('--broadcast')
        wait('default profile automatic publishing', self.live)
        automatic_source = self.control('status')['source_id']
        require(uuid.UUID(automatic_source).version == 4, 'automatic source is not a UUID v4')
        self.inspect(public, [private, hidden])
        require(b'Local browser URL' not in automatic.bytes() and
                b'Read-only audience browser:' not in automatic.bytes(), 'turnkey activated local web listener')
        quit_presenter(automatic)
        wait('owned default publication stops on quit', lambda: self.status()['state'] == 'offline')
        self.cases.append('bare-broadcast-default-profile-auto-uuid-no-listener-owned-stop')

        named = launch('--broadcast-profile', 'acceptance')
        wait('explicit named profile publishing', self.live)
        named_source = self.control('status')['source_id']
        require(named_source != automatic_source, 'turnkey reused previous source identity')
        self.inspect(public, [private, hidden])
        replacement_source = str(uuid.uuid4())
        replacement = launch('--broadcast', replacement_source)
        wait('legacy replacement registration only', lambda: b': registered;' in replacement.bytes())
        require(self.control('status')['source_id'] == named_source, 'legacy UUID unexpectedly started publishing')
        # Explicitly stop before starting the replacement: neither start path
        # may steal an active publication. The old token remains fenced.
        self.control('stop')
        self.control('start', self.url, 'main', str(self.key), replacement_source)
        wait('replacement profile publishing', self.live)
        require(self.control('status')['source_id'] == replacement_source, 'profile source selection failed')
        quit_presenter(named)
        # Wait beyond the old caller source lease: its teardown must not stop the
        # newly owned stream, even though both used the same destination profile.
        deadline = time.monotonic() + 7
        while time.monotonic() < deadline:
            require(self.live() and self.control('status')['source_id'] == replacement_source,
                    'superseded presenter stopped unrelated publication')
            time.sleep(.3)
        self.inspect(public, [private, hidden])
        self.control('stop')
        wait('replacement publication stopped', lambda: self.status()['state'] == 'offline')
        quit_presenter(replacement)
        self.cases.append('named-profile-legacy-registration-superseded-token-teardown-fenced')

        missing = launch('--broadcast-profile', 'missing-profile')
        wait('missing profile content-free diagnostic', lambda: b'local playback continues' in missing.bytes())
        require(self.control('status')['local'] in ('offline', 'stopped'), 'missing profile opened upload gate')
        require(missing.child.poll() is None, 'missing profile stopped local playback')
        quit_presenter(missing)
        self.inline_broadcast()
        self.cases.append('missing-profile-no-default-fallback-local-playback-independent')
        secret = self.key.read_bytes().strip()
        require(all(secret not in mirror.bytes() for mirror in (automatic, named, replacement, missing)),
                'publisher key leaked into turnkey terminal')

    def inline_broadcast(self):
        import termios
        # Use the same real terminal fixture and selected isolated daemon.
        mirror = PRESENTER.Mirror(self.command + ['broadcast', 'start'], self.env)
        self.terminals.append(mirror)
        wait('inline source picker', lambda: b'Choose a terminal' in mirror.bytes())
        # Exercise ordinary keys, not paste: raw policy previously ignored
        # all typing, and a one-session fixture still appeared to pass.
        mirror.send(b'no-such-terminal')
        time.sleep(.3)
        mirror.send(b'\r')
        time.sleep(.3)
        require(b'Review public sharing' not in mirror.bytes(),
                'typing did not filter out the unmatched terminal')
        mirror.send(b'\x15')  # Readline clear query.
        mirror.send(b'terminal-producer')
        time.sleep(.3)
        mirror.send(b'\r')
        wait('inline review', lambda: b'Review public sharing' in mirror.bytes())
        require(self.control('status')['local'] in ('offline', 'stopped'),
                'merely selecting a terminal published output')
        # A second Enter confirms the separate public-sharing review.
        mirror.send(b'\r')
        wait('inline start finished', lambda: mirror.child.poll() is not None, 40)
        require(mirror.child.returncode == 0, 'inline broadcast did not reach first frame acknowledgement')
        require(self.live(), 'inline start returned without live frame')
        wait('inline terminal reaches browser', self.inspect_terminal)
        output = mirror.bytes()
        require(b'\x1b[?1049h' not in output and b'\x1b[?47h' not in output,
                'inline broadcast entered alternate screen')
        require(b'\x1b[?2004h' in output and b'\x1b[?2004l' in output,
                'inline paste mode lifecycle missing')
        modes = termios.tcgetattr(mirror.fd)
        require(modes[3] & termios.ICANON and modes[3] & termios.ECHO,
                'inline broadcast left terminal in raw mode')
        self.control('pause')
        wait('inline pause', lambda: self.status()['state'] == 'paused')
        self.control('resume')
        wait('inline resume', self.live)
        self.control('stop')
        wait('inline stop', lambda: self.status()['state'] == 'offline')
        cancelled = PRESENTER.Mirror(self.command + ['broadcast', 'start'], self.env)
        self.terminals.append(cancelled)
        wait('inline cancellation picker', lambda: b'Choose a terminal' in cancelled.bytes())
        import fcntl
        import struct
        fcntl.ioctl(cancelled.fd, termios.TIOCSWINSZ, struct.pack('HHHH', 10, 50, 0, 0))
        os.kill(cancelled.child.pid, signal.SIGWINCH)
        cancelled.send(b'\x1b[200~no-such-terminal\x1b[201~')
        time.sleep(.25)
        cancelled.send(b'\x1b')
        wait('inline cancelled cleanly', lambda: cancelled.child.poll() is not None)
        require(self.control('status')['local'] in ('offline', 'stopped'),
                'cancelled source picker changed broadcast state')
        modes = termios.tcgetattr(cancelled.fd)
        require(modes[3] & termios.ICANON and modes[3] & termios.ECHO,
                'cancelled inline picker left raw terminal modes')
        self.cases.append('inline-resize-paste-cancel-no-publication-terminal-restoration')
        self.cases.append('inline-picker-review-confirm-first-frame-browser-pause-resume-stop-terminal-restoration')

    def timer_and_idle(self):
        self.stage = 'native-event-driven-idle-and-timer'
        time.sleep(1)  # Settle initial capture and observer admission.
        before = self.stack.metrics()['bmux_relay_upload_bytes_total']
        wire = self.cdp.evaluate('__wire.length')
        canvas = self.cdp.evaluate("document.querySelector('#live-broadcast canvas:not([hidden])').toDataURL()")
        time.sleep(2)
        require(self.stack.metrics()['bmux_relay_upload_bytes_total'] == before, 'idle native source uploaded frames')
        require(self.cdp.evaluate('__wire.length') == wire, 'idle browser received binary frames')
        require(self.cdp.evaluate("document.querySelector('#live-broadcast canvas:not([hidden])').toDataURL()") == canvas, 'idle pixels changed')
        self.producer_mode.write_text('timer')
        wait('native timer frame', lambda: self.cdp.evaluate('__wire.length') >= wire + 4)
        time.sleep(.4)
        records = self.cdp.evaluate('__wire.slice(' + str(wire + 1) + ')')
        require(len(records) >= 3 and all(row['bytes'] < 1024 for row in records), 'timer emitted a full-row/full-image bundle')
        require(all(b['time'] - a['time'] >= 15 for a, b in zip(records, records[1:])), 'native timer exceeded 60 FPS')
        require(self.cdp.evaluate("document.querySelector('#live-broadcast canvas:not([hidden])').toDataURL()") != canvas, 'native timer did not change pixels')
        after = self.stack.metrics()['bmux_relay_upload_bytes_total']
        require(0 < after - before < 12000, 'native timer source-to-relay upload not localized')
        self.cases.append('native-idle-zero-frame-traffic-timer-tiny-both-hops-60FPS-ceiling')

    def verify_observed_resizing(self, target):
        """Normal attach sizes the PTY; observation follows without competing."""
        local = PRESENTER.Mirror(self.command + ['attach', target], self.env)
        self.terminals.append(local)
        wait('normal terminal attached', lambda: b'NATIVE_TERMINAL_PUBLIC_5831' in local.bytes())

        def progressing(label, geometry=None):
            wait(label + ' visible', self.inspect_terminal)
            if geometry is not None:
                cols, rows = geometry
                wait(label + ' authoritative canvas geometry', lambda: self.cdp.evaluate(f"Array.from(document.querySelectorAll('#live-broadcast canvas')).some(c=>!c.hidden && c.width === {cols * 10} && c.height === {rows * 20})"))
            inspected = self.inspect('NATIVE_TERMINAL_PUBLIC_5831', [])
            require(inspected['info']['generation'].endswith(':' + target), 'tab selection silently retargeted public source')
            previous, _ = self.frame()
            revision = int(self.status()['revision'])
            wait(label + ' changing frame', lambda: int(self.status()['revision']) > revision and self.frame()[0] != previous)
            # Require sustained progress, not just the single resize repair.
            time.sleep(.5)
            previous, _ = self.frame()
            canvas = self.cdp.evaluate("Array.from(document.querySelectorAll('#live-broadcast canvas')).find(c=>!c.hidden)?.toDataURL()")
            revision = int(self.status()['revision'])
            wait(label + ' sustained progress', lambda: int(self.status()['revision']) > revision and self.frame()[0] != previous)
            wait(label + ' audience canvas changes', lambda: self.cdp.evaluate("Array.from(document.querySelectorAll('#live-broadcast canvas')).find(c=>!c.hidden)?.toDataURL()") != canvas)
            return True

        progressing('120x40 attach over 80x24 observer', (120, 40))
        for cols, rows in [(100, 32), (60, 18), (120, 40), (80, 24)]:
            fcntl.ioctl(local.fd, termios.TIOCSWINSZ, struct.pack('HHHH', rows, cols, 0, 0))
            progressing(f'{cols}x{rows} resize', (cols, rows))
        local.stop()
        self.terminals.remove(local)
        progressing('detached observer retains size')
        local = PRESENTER.Mirror(self.command + ['attach', target], self.env)
        self.terminals.append(local)
        progressing('reattached 120x40')
        self.run('split-pane', '--session', target, '--direction', 'vertical')
        progressing('split pane layout')
        self.run('new-tab', '--session', target)
        progressing('new tab selection')
        tabs = self.run('list-tabs', '--json').stdout
        tab_values = json.loads(tabs)
        if isinstance(tab_values, dict):
            tab_values = tab_values.get('tabs', tab_values.get('contexts', []))
        require(bool(tab_values), 'list-tabs returned no stable selection')
        tab = tab_values[0]
        tab_id = tab.get('context_id', tab.get('id'))
        require(bool(tab_id), 'list-tabs missing stable identity')
        self.run('switch-tab', str(tab_id), '--session', target)
        progressing('return to first tab')
        self.control('pause')
        wait('resized observation pause clears', lambda: self.status()['state'] == 'paused' and self.cdp.evaluate("!Array.from(document.querySelectorAll('#live-broadcast canvas')).some(c=>!c.hidden)"))
        fcntl.ioctl(local.fd, termios.TIOCSWINSZ, struct.pack('HHHH', 32, 100, 0, 0))
        time.sleep(.5)
        require(self.stack.snapshot()[1] is None, 'resize bypassed paused audience gate')
        self.control('resume')
        progressing('resumed at changed geometry')
        local.stop()
        self.terminals.remove(local)
        self.cases.append('non-sizing-observer-mismatch-resize-detach-reattach-pause-resume-sustained-frames')

    def inspect_terminal(self):
        try:
            # Validate the mounted audience viewer, not just the independent
            # diagnostic decoder. A continuously advancing producer previously
            # decoded fine in isolation while the page rejected every frame.
            if not self.cdp.evaluate("document.getElementById('live-status')?.textContent === 'Live · read-only' && Array.from(document.querySelectorAll('#live-broadcast canvas')).some(c => !c.hidden && c.width > 0 && c.height > 0)"):
                return False
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
        if self.stack:
            self.stack.close()
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
        if self.browser_temp is not None:
            self.browser_temp.cleanup()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--relay', required=True, type=Path)
    parser.add_argument('--wire-tool', required=True, type=Path)
    parser.add_argument('--binary', required=True, type=Path)
    parser.add_argument('--sha256', required=True)
    parser.add_argument('--assets', required=True, type=Path)
    parser.add_argument('--browser', required=True, type=Path)
    args = parser.parse_args()
    args.binary, args.assets, args.browser = args.binary.resolve(), args.assets.resolve(), args.browser.resolve()
    args.relay, args.wire_tool = args.relay.resolve(), args.wire_tool.resolve()
    require((args.assets / 'manifest.json').is_file(),
            '--assets must name the immutable live-assets/<bundle_sha256> directory, not raw target/web-assets')
    LOCAL.verify_assets(args.assets)
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
                              'trace': [{'file': Path(frame.filename).name, 'line': frame.lineno, 'function': frame.name}
                                        for frame in traceback.extract_tb(error.__traceback__)],
                              'passed_cases': harness.cases, 'viewer_diagnostic': getattr(harness, 'viewer_diagnostic', None)}, sort_keys=True))
            return 1
        finally:
            harness.close()
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
