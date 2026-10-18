#!/usr/bin/env python3
"""Actual generated-site, Rust relay WSS and WASM pixel/lifecycle acceptance; no builds."""
import argparse
import base64
import hashlib
import importlib.util
import json
from pathlib import Path
import subprocess
import tempfile
import time
import urllib.request

ROOT = Path(__file__).resolve().parent.parent

def load(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec); spec.loader.exec_module(module)
    return module

CDP = load('browser_cdp', ROOT / 'scripts/web-browser-cdp.py')
LOCAL = load('browser_local', ROOT / 'scripts/smoke-live-local.py')
wait = LOCAL.wait

INSTRUMENT = r'''
window.__proof={texts:[],images:0,frames:[],urls:[],acks:[],paints:0};
const fill=CanvasRenderingContext2D.prototype.fillText;
CanvasRenderingContext2D.prototype.fillText=function(text,...args){__proof.texts.push(String(text));return fill.call(this,text,...args)};
const image=CanvasRenderingContext2D.prototype.drawImage;
CanvasRenderingContext2D.prototype.drawImage=function(...args){__proof.images++;return image.apply(this,args)};
const NativeSocket=window.WebSocket;
window.WebSocket=class extends NativeSocket {
 constructor(url,...args){super(url,...args);__proof.urls.push(String(url));this.addEventListener('message',event=>{
  if(event.data instanceof ArrayBuffer)__proof.frames.push({bytes:event.data.byteLength,time:performance.now()});
 });}
 send(data){if(typeof data==='string'){const value=JSON.parse(data);if(value.type==='ack')__proof.acks.push({...value,time:performance.now()})}return super.send(data)}
};
'''
PIXELS = r'''(() => {
 const c=document.querySelector('#live-broadcast canvas:not([hidden])');if(!c)return null;
 const x=c.getContext('2d'),sample=(a,b)=>Array.from(x.getImageData(a,b,1,1).data);
 const d=x.getImageData(20,20,700,20).data;let bright=0;
 for(let i=0;i<d.length;i+=4)if(d[i]>80||d[i+1]>80||d[i+2]>80)bright++;
 return {width:c.width,height:c.height,brightTitlePixels:bright,red:sample(30,150),green:sample(90,150),
 blue:sample(30,210),imageDrawCalls:__proof.images,text:__proof.texts.join('')};
})()'''


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ('browser', 'site', 'fixture', 'relay', 'wire-tool'):
        parser.add_argument('--' + name, required=True, type=Path)
    parser.add_argument('--evidence', type=Path, help='Write pixel screenshots and bounded JSON here')
    args = parser.parse_args()
    site = args.site.resolve(); version = (site / 'live-assets/current-version').read_text().strip()
    assets = site / 'live-assets' / version
    frame = args.fixture.read_bytes(); assert frame.startswith(b'BMWP\x04')
    module_path = f'/live-assets/{version}/live-page.js'
    pages = list(site.rglob('*.html')); assert pages
    for page in pages:
        if not page.is_relative_to(site / 'live-assets') and not page.is_relative_to(site / 'web-assets'):
            assert module_path in page.read_text(), 'generated global viewer mount missing'
    LOCAL.verify_assets(assets)
    timer = subprocess.run([str(args.wire_tool.resolve()), '--timer'], input=frame, capture_output=True, check=True, timeout=5).stdout
    delta = subprocess.run([str(args.wire_tool.resolve()), '--delta', str(args.fixture.resolve())], input=timer, capture_output=True, check=True, timeout=5).stdout
    assert delta.startswith(b'BMWP\x04') and len(delta) < 256 and len(delta) < len(frame) / 10, 'one digit must be a tiny patch'
    evidence = {'cases': [], 'relay_sha256': hashlib.sha256(args.relay.read_bytes()).hexdigest(), 'delta_bytes': len(delta), 'full_bytes': len(frame)}
    if args.evidence: args.evidence.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix='bmux-live-browser-') as directory:
        root = Path(directory); stack = LOCAL.RelayStack(args.relay, root, assets, site)
        browser = cdp = None
        log = open(root / 'browser.log', 'wb')
        try:
            browser = subprocess.Popen([str(args.browser.resolve()), '--headless', '--no-sandbox', '--disable-gpu',
                '--disable-background-networking', '--no-first-run', '--no-default-browser-check', '--no-proxy-server',
                '--ignore-certificate-errors', '--host-resolver-rules=' + stack.chrome_rules,
                '--remote-debugging-port=0', '--remote-allow-origins=*', '--user-data-dir=' + str(root / 'profile'), 'about:blank'],
                stdin=subprocess.DEVNULL, stdout=log, stderr=log)
            ready = root / 'profile/DevToolsActivePort'; wait(lambda: ready.exists() and browser.poll() is None)
            port = ready.read_text().splitlines()[0]
            with urllib.request.urlopen(f'http://127.0.0.1:{port}/json/list', timeout=5) as response:
                target = next(item for item in json.load(response) if item['type'] == 'page')
            cdp = CDP.CDP(target['webSocketDebuggerUrl']); cdp.call('Page.enable'); cdp.call('Runtime.enable')
            cdp.call('Page.addScriptToEvaluateOnNewDocument', {'source': INSTRUMENT})
            cdp.call('Page.navigate', {'url': 'https://bmux.dev/live/'})
            wait(lambda: cdp.evaluate("!!document.getElementById('live-broadcast')"))
            visible = lambda: cdp.evaluate("!!document.querySelector('#live-broadcast canvas:not([hidden])')")
            hidden = lambda: cdp.evaluate("document.querySelectorAll('#live-broadcast canvas').length===0")
            stack.control('start'); first_ack = stack.publish(frame); wait(visible)
            proof = cdp.evaluate(PIXELS)
            assert proof['width'] == 800 and proof['height'] == 480
            assert proof['brightTitlePixels'] > 30 and 'bmux_tui' in proof['text'] and 'browser' in proof['text']
            assert proof['imageDrawCalls'] > 0 and proof['red'][0] > 220 and proof['green'][1] > 220 and proof['blue'][2] > 220
            evidence['canvas'] = proof; evidence['cases'].append('generated-live-real-WASM-text-image-pixels')
            if args.evidence:
                (args.evidence / 'full.png').write_bytes(base64.b64decode(cdp.call('Page.captureScreenshot', {'format': 'png'})['data']))
            cdp.evaluate("window.__beforePixels=document.querySelector('#live-broadcast canvas:not([hidden])').getContext('2d').getImageData(0,0,800,480).data.slice();true")
            slow = stack.ws()
            assert slow.receive()['type'] == 'state' and slow.receive() == frame
            count = cdp.evaluate('__proof.frames.length'); stack.publish(delta)
            wait(lambda: cdp.evaluate('__proof.frames.length') > count)
            changed = wait(lambda: cdp.evaluate(r'''(()=>{const c=document.querySelector('#live-broadcast canvas:not([hidden])');
              if(!c)return null;const d=c.getContext('2d').getImageData(0,0,800,480).data;let n=0,minX=800,minY=480,maxX=0,maxY=0;
              for(let i=0;i<d.length;i+=4)if(d[i]!==__beforePixels[i]||d[i+1]!==__beforePixels[i+1]||d[i+2]!==__beforePixels[i+2]){
                const x=(i/4)%800,y=Math.floor(i/4/800);n++;minX=Math.min(minX,x);maxX=Math.max(maxX,x);minY=Math.min(minY,y);maxY=Math.max(maxY,y)}
              return n?{pixels:n,minX,minY,maxX,maxY,bytes:__proof.frames.at(-1).bytes}:null})()'''))
            assert changed['bytes'] == len(delta), 'fast viewer did not receive the original delta'
            assert changed['minX'] >= 750 and changed['maxX'] < 790 and changed['minY'] >= 440 and changed['maxY'] < 460
            slow.send(first_ack)
            repaired = slow.receive()
            while isinstance(repaired, dict): repaired = slow.receive()
            assert repaired == delta or repaired == timer, 'slow peer received no coherent continuation'
            # Explicit browser decoder loss must always request a complete repair.
            slow.send({'type': 'repair'})
            repaired = slow.receive()
            while isinstance(repaired, dict): repaired = slow.receive()
            assert repaired == timer, 'repair did not materialize retained full snapshot'
            slow.close()
            evidence['cases'].append('slow-viewer-credit-and-explicit-full-baseline-repair')
            evidence['timer_pixels'] = changed; evidence['cases'].append('one-digit-localized-pixels-and-tiny-both-hop-delta')
            if args.evidence:
                (args.evidence / 'timer.png').write_bytes(base64.b64decode(cdp.call('Page.captureScreenshot', {'format': 'png'})['data']))
            before = cdp.evaluate('({frames:__proof.frames.length,text:__proof.texts.length,images:__proof.images})')
            time.sleep(2); stack.control('heartbeat'); time.sleep(.5)
            assert cdp.evaluate('({frames:__proof.frames.length,text:__proof.texts.length,images:__proof.images})') == before, 'idle produced frame or paint work'
            evidence['cases'].append('idle-zero-binary-and-zero-canvas-paint-heartbeat-only')
            # Burst publisher ACKs may outpace rendering. The relay must coalesce
            # to a coherent repair, and the real browser decoder must reach latest.
            burst = []; previous = timer
            basefile = root / 'burst-base.bmwp'
            for _ in range(12):
                following = subprocess.run([str(args.wire_tool.resolve()), '--timer'], input=previous,
                    capture_output=True, check=True, timeout=5).stdout
                basefile.write_bytes(previous)
                patch = subprocess.run([str(args.wire_tool.resolve()), '--delta', str(basefile)], input=following,
                    capture_output=True, check=True, timeout=5).stdout
                assert len(patch) < 256
                burst.append(patch); previous = following
            start_ack = cdp.evaluate('__proof.acks.length')
            for patch in burst: last_ack = stack.publish(patch)
            wait(lambda: cdp.evaluate('__proof.acks.at(-1)?.revision') == last_ack['revision'])
            admitted = cdp.evaluate('__proof.acks.slice(' + str(start_ack) + ')')
            assert admitted, 'burst produced no admitted browser frames'
            gaps = [b['time'] - a['time'] for a, b in zip(admitted, admitted[1:])]
            assert all(gap >= 15 for gap in gaps), (
                f'browser exceeded 60 FPS: {len(admitted)} admissions, minimum gap {min(gaps):.3f} ms')
            evidence['burst_admitted'] = len(admitted)
            evidence['cases'].append('burst-bounded-60FPS-latest-baseline-coherent-real-decoder')
            stack.control('pause'); wait(hidden); time.sleep(.3); assert hidden()
            evidence['cases'].append('pause-clears')
            stack.control('resume'); stack.publish(frame); wait(visible)
            evidence['cases'].append('resume-identical-source-full-repair')
            stack.control('stop'); wait(hidden); evidence['cases'].append('stop-clears')
            stack.control('start'); stack.publish(frame); wait(visible)
            evidence['cases'].append('replacement-epoch-reconnect')
            wait(hidden, 18); evidence['cases'].append('actual-relay-lease-expiry-clears')
            stack.control('start'); stack.publish(frame); wait(visible)
            evidence['cases'].append('reconnect-after-expiry')
            urls = cdp.evaluate('__proof.urls'); assert urls and all(url == 'wss://live.bmux.dev/v1/viewer/ws' for url in urls)
            assert all(not row['authorization'] for row in stack.network if row['path'] == '/v1/viewer/ws')
            evidence['cases'].append('WSS-output-only-no-browser-publisher-credentials')
            cdp.call('Page.navigate', {'url': 'https://bmux.dev/'})
            wait(lambda: cdp.evaluate("document.readyState==='complete'&&!document.getElementById('live-broadcast')"))
            # A new binary would render if the previous live page had leaked its mount.
            stack.publish(delta); time.sleep(.4)
            assert cdp.evaluate("document.querySelectorAll('#live-broadcast canvas').length===0")
            evidence['cases'].append('navigation-unmount-disposes-viewer')
            evidence['result'] = 'PASS'
        finally:
            if cdp: cdp.close()
            if browser:
                browser.terminate()
                try: browser.wait(timeout=5)
                except subprocess.TimeoutExpired: browser.kill(); browser.wait(timeout=5)
            stack.close(); log.close()
    if args.evidence: (args.evidence / 'report.json').write_text(json.dumps(evidence, sort_keys=True))
    print(json.dumps(evidence, sort_keys=True))

if __name__ == '__main__': main()
