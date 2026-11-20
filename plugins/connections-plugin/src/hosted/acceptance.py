#!/usr/bin/env python3
"""Local hosted product acceptance. Pass the absolute bundled bmux binary.

Requires Python 3.11+, an adjacent pane supervisor, and zbarimg or macOS Vision.
The clipboard provider is disabled before any invocation; use only fixture auth.
"""
import http.server
import json
import os
from pathlib import Path
import shutil
import struct
import subprocess
import sys
import tempfile
import threading
import tomllib
import zlib

HERE = Path(__file__).resolve().parent
PTY = HERE.parents[3] / 'scripts/smoke-pty.py'
TOKEN = 'synthetic-hosted-acceptance'


def decode_qr(rows, root):
    # Each dense terminal cell encodes two modules. Decode independently of qrcode.
    scan = bytearray()
    for row in rows:
        assert len(row) == len(rows[0])
        for dark in ('▀█', '▄█'):
            pixels = b''.join(bytes([0 if c in dark else 255]) * 8 for c in row)
            scan.extend((b'\0' + pixels) * 8)
    def chunk(tag, payload):
        return struct.pack('>I', len(payload)) + tag + payload + struct.pack('>I', zlib.crc32(tag + payload))
    image = root / 'qr.png'
    header = struct.pack('>IIBBBBB', len(rows[0])*8, len(rows)*16, 8, 0, 0, 0, 0)
    image.write_bytes(b'\x89PNG\r\n\x1a\n' + chunk(b'IHDR', header) + chunk(b'IDAT', zlib.compress(scan)) + chunk(b'IEND', b''))
    if shutil.which('zbarimg'):
        cmd = ['zbarimg', '--quiet', '--raw', str(image)]
    else:
        # Vision is supplied by macOS; no developer tools or network needed.
        script = """ObjC.import('Vision'); function run(args) {
            const request = $.VNDetectBarcodesRequest.alloc.init;
            const handler = $.VNImageRequestHandler.alloc.initWithURLOptions($.NSURL.fileURLWithPath(args[0]), $({}));
            const error = Ref();
            if (!handler.performRequestsError($([request]), error)) throw Error(ObjC.unwrap(error[0].localizedDescription));
            const codes = request.results;
            if (Number(codes.count) !== 1) throw Error('expected one QR code, got ' + codes.count);
            return ObjC.unwrap(codes.objectAtIndex(0).payloadStringValue);
        }"""
        cmd = ['/usr/bin/osascript', '-l', 'JavaScript', '-e', script, str(image)]
    return subprocess.check_output(cmd, timeout=90, text=True).strip()


def run(binary, root):
    dirs = {n: root/n for n in ('home','config','data','state','runtime','log','tmp')}
    for d in dirs.values(): d.mkdir(mode=0o700)
    requests = []
    class Plane(http.server.BaseHTTPRequestHandler):
        def log_message(self, *args): pass
        def respond(self, status, body):
            body = json.dumps(body).encode()
            self.send_response(status)
            self.send_header('Content-Type', 'application/json')
            self.send_header('Content-Length', str(len(body)))
            self.end_headers()
            self.wfile.write(body)
        def do_POST(self):
            body = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
            requests.append(('POST',self.path,self.headers.get('Authorization'),body))
            self.respond(409 if len(requests)==1 else 200, {'name':'demo-2','url':url})
        def do_DELETE(self):
            requests.append(('DELETE',self.path,self.headers.get('Authorization'),None))
            self.respond(200,{})
    with http.server.HTTPServer(('127.0.0.1',0),Plane) as plane:
        origin = f'http://127.0.0.1:{plane.server_port}'
        url = origin + '/invite/demo-2'
        threading.Thread(target=plane.serve_forever,daemon=True).start()
        config = dirs['config']/'bmux.toml'
        config.write_text('[plugins]\ndisabled = ["bmux.clipboard"]\n[connections]\ndefault_target = "local"\n[keybindings]\ninitial_mode = "normal"\n')
        (dirs['runtime']/'auth-state.json').write_text(json.dumps({'access_token':TOKEN}))
        env = {'HOME':str(dirs['home']), 'TMPDIR':str(dirs['tmp']), 'TERM':'xterm-256color', 'SHELL':'/bin/sh', 'PATH':'/usr/bin:/bin', 'BMUX_CONFIG':str(config), 'BMUX_CONTROL_PLANE_URL':origin, 'NO_COLOR':'1', 'TOKIO_WORKER_THREADS':'2'}
        env.update({f'BMUX_{n.upper()}_DIR':str(dirs[n]) for n in ('config','runtime','data','state','log')})
        prefix = [str(binary)]
        def cli(*args, code=0):
            p=subprocess.run(prefix+list(args), cwd=root, env=env, stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=45)
            assert p.returncode==code,(args,p.returncode,p.stdout,p.stderr)
            return p
        def connections(): return tomllib.loads(config.read_text())['connections']
        out=cli('share','--name','demo','--ttl','30m','--one-time','--copy','--qr')
        assert 'warning: clipboard copy failed:' in out.stderr and 'clipboard service unavailable' in out.stderr,out.stderr
        assert f'Invite URL: {url}\n' in out.stdout and 'Share link: bmux://demo-2\n' in out.stdout,out.stdout
        for i,name in enumerate(('demo','demo-2')):
            assert requests[i]==('POST','/v1/share-links','Bearer '+TOKEN,{'name':name,'target':'local','role':'control','ttl':'30m','one_time':True}),requests
        assert connections()['share_links']=={'demo-2':'local'}
        assert decode_qr(out.stdout.split('QR:\n')[1].splitlines(),root)==url
        cli('share','revoke','demo-2')
        assert len(requests)==3 and requests[-1]==('DELETE','/v1/share-links/demo-2','Bearer '+TOKEN,None),requests
        assert not connections().get('share_links')
        print('PASS share fallback, URL, decoded QR, disabled-clipboard warning, persisted revoke',flush=True)
        plane.shutdown()
        try:
            cli('server','start','--daemon')
            cli('new-session','acceptance')
            cmd = [sys.executable,str(PTY),'--transcript',str(root/'terminal.log'),'--timeout','40','--',*prefix,'connect','local','acceptance']
            result=subprocess.run(cmd,cwd=root,env=env,timeout=50)
            assert result.returncode==0,(root/'terminal.log').read_bytes()[-2000:]
            assert connections()['recent_sessions']['local'][0]=='acceptance'
            print('PASS connect: live PTY paint/detach and session recency',flush=True)
            failure=cli('connect','tls://127.0.0.1:not-a-port','acceptance',code=1)
            assert 'invalid TLS port' in failure.stderr and 'panic' not in failure.stderr,failure.stderr
            missing=cli('connect','local','absent-fixture-session',code=1)
            assert 'session' in missing.stderr.lower() and 'panic' not in missing.stderr,missing.stderr
            assert connections()['recent_sessions']['local'][0]=='acceptance'
        finally:
            cli('server','stop')
        print('PASS connect error',flush=True)


if __name__=='__main__':
    binary=Path(sys.argv[1]).resolve(strict=True)
    with tempfile.TemporaryDirectory(prefix='hc-',dir='/tmp') as tmp:
        run(binary,Path(tmp).resolve())
