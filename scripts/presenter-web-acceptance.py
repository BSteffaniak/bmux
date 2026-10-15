#!/usr/bin/env python3
"""Real presenter browser acceptance. No build/install; all assertions mandatory.

Pending integration contract: present play/host --web-assets PATH [--web-only],
shared --web-{port,auth,host,public-host,allowed-hosts}, [presenter.web] mode/assets.
Run only after coordinator grants the browser/runtime resource lock. Evidence is
private, retained, and capability-safe in console output. Linux /proc required.
"""
import argparse
import base64
import fcntl
import importlib.util
import json
import os
from pathlib import Path
import pty
import re
import signal
import socket
import struct
import subprocess
import sys
import tempfile
import termios
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
import zlib

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parent.parent


def load(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


# Reuse read-only transport/pixel helpers, never invoke the attach suite.
WEB = load("presenter_web_helpers", ROOT / "scripts/web-browser-acceptance.py")
CONTROL = load("presenter_host_control", ROOT / "plugins/presenter-plugin/tests/acceptance.py")
require, wait_for = WEB.require, WEB.wait_for
SECRET = "PRESENTER_WEB_PRIVATE_9327"
HIDDEN = "PRESENTER_WEB_UNREVEALED_9327"
FIRST = "PRESENTER_WEB_FIRST_9327"
LAST = "FINAL_SLIDE_COMPLETE_ZYXWVUT"
REFLOW = "AUDIENCE_REFLOW_SENTENCE_9327"
PRIVATE_CONFIG = "AUDIENCE_PRIVATE_CUSTOM_CONFIG_9327"

# Instrument transport only: the product adapter, WASM layout, and Canvas commit
# remain real. Faults affect one viewer; no test server or alternative renderer.
DOCUMENT_PROBE = r"""
(() => {
 const original = window.fetch.bind(window);
 window.__audienceProbe = {resources: {}, fault: null, faults: 0, failures: []};
 const watch = () => {
   const status = document.getElementById('status');
   if (!status) return;
   new MutationObserver(() => {
     if (status.textContent.includes('retained')) window.__audienceProbe.failures.push(status.textContent);
   }).observe(status, {childList:true, subtree:true, characterData:true});
 };
 addEventListener('DOMContentLoaded', watch, {once:true});
 window.fetch = async (...args) => {
   const path = new URL(typeof args[0] === 'string' ? args[0] : args[0].url, location.href).pathname;
   const p = window.__audienceProbe;
   if (path.startsWith('/resource/')) {
     p.resources[path] = (p.resources[path] || 0) + 1;
     if (p.fault === 'resource' && path.endsWith('0'.repeat(64))) return new Response('', {status:503});
   }
   const response = await original(...args);
   if (path !== '/document' || response.status !== 200 || !p.fault) return response;
   const value = await response.clone().json();
   p.faults++;
   const headers = new Headers(response.headers);
   if (p.fault === 'invalid') value.content = [123];
   if (p.fault === 'stale') {
     value.publication_revision = (BigInt(value.publication_revision) + 100n).toString();
     value.revision = '1';
     headers.set('X-Snapshot-Revision', value.publication_revision);
   }
   if (p.fault === 'resource') {
     const content = new TextDecoder().decode(new Uint8Array(value.content));
     const image = value.resources.find(r => r.mime_type === 'image/png');
     if (!image) throw Error('acceptance fixture has no PNG resource');
     value.content = Array.from(new TextEncoder().encode(content.split(image.id).join('0'.repeat(64))));
     image.id = '0'.repeat(64);
   }
   if (!p.baseline) {
     const digest = await crypto.subtle.digest('SHA-256', new Uint8Array(value.content));
     p.candidate = {generation:value.generation, revision:value.revision,
       publication_revision:value.publication_revision,
       content_sha256:Array.from(new Uint8Array(digest), b=>b.toString(16).padStart(2,'0')).join('')};
     p.baseline = {inspection:window.__bmuxDocumentInspection(), pixels:window.__audiencePixels()};
   }
   return new Response(JSON.stringify(value), {status:200, headers});
 };
})();
"""


class Mirror:
    """Actual controlling PTY, bounded private capture; no user terminal involved."""
    def __init__(self, argv, env):
        self.fd, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 120, 0, 0))

        def terminal():
            os.setsid()
            fcntl.ioctl(0, termios.TIOCSCTTY, 0)

        try:
            self.child = subprocess.Popen(argv, env=env, stdin=slave, stdout=slave,
                                          stderr=slave, preexec_fn=terminal)
        finally:
            os.close(slave)
        self.output = bytearray()
        self.lock = threading.Lock()
        self.thread = threading.Thread(target=self.drain, daemon=True)
        self.thread.start()

    def drain(self):
        while True:
            try:
                data = os.read(self.fd, 65536)
            except OSError:
                return
            if not data:
                return
            with self.lock:
                self.output.extend(data)
                del self.output[:-1024 * 1024]

    def bytes(self):
        with self.lock:
            return bytes(self.output)

    def send(self, value):
        os.write(self.fd, value)

    def stop(self):
        if self.child.poll() is None:
            os.killpg(self.child.pid, signal.SIGTERM)
            self.child.wait(timeout=8)
        self.thread.join(timeout=2)
        os.close(self.fd)


class Acceptance:
    def __init__(self, args):
        self.args = args
        self.root = Path(tempfile.mkdtemp(prefix="pw-", dir=args.state_parent))
        self.root.chmod(0o700)
        print("Private evidence/state: " + str(self.root), flush=True)
        self.env = {k: v for k, v in os.environ.items() if k in ("PATH", "LANG", "LC_ALL", "TZ")}
        self.env.setdefault("PATH", "/usr/bin:/bin")
        for name in ("home", "config", "runtime", "data", "state", "logs", "cache", "tmp"):
            (self.root / name).mkdir(mode=0o700)
        for key, name in (("HOME", "home"), ("TMPDIR", "tmp"),
                          ("XDG_CONFIG_HOME", "config"), ("XDG_RUNTIME_DIR", "runtime"),
                          ("XDG_DATA_HOME", "data"), ("XDG_STATE_HOME", "state"),
                          ("XDG_CACHE_HOME", "cache")):
            self.env[key] = str(self.root / name)
        for name in ("config", "runtime", "data", "state", "log"):
            self.env["BMUX_" + name.upper() + "_DIR"] = str(self.root / ("logs" if name == "log" else name))
        self.config = self.root / "config/bmux.toml"
        self.env.update(BMUX_CONFIG=str(self.config), BMUX_NO_BASE_CONFIG="1",
                        BMUX_RUNTIME_NAME="default", BMUX_TARGET="local", TERM="xterm-256color",
                        BMUX_PRESENTER_WEB_DELIVERY="frame")
        self.command = [str(args.binary), "--runtime", "default", "--target", "local",
                        "--config", str(self.config)]
        self.processes, self.clients, self.cases = [], [], []
        self.browser_processes = []
        self.failed = None
        self.browser_sequence = 0
        self.write_config()
        self.deck = self.root / "deck.md"
        self.prepare_deck()
        self.credential = os.urandom(32).hex()
        (self.root / "presenter").write_text(self.credential)
        (self.root / "presenter").chmod(0o600)
        self.identity = {"binary": WEB.digest(args.binary), "browser": WEB.digest(args.browser),
                         "assets": {name: WEB.digest(args.assets / name) for name in
                                    (*WEB.ASSETS, "presenter-adapter.js", "bmux_presenter.js", "bmux_presenter_bg.wasm")},
                         "plugins": {str(p.relative_to(args.plugins)): WEB.digest(p)
                                     for p in sorted(args.plugins.rglob("*.so"))}}
        require(self.identity["plugins"], "no staged plugin libraries")

    def write_config(self, extra=""):
        self.config.write_text('[plugins]\nenabled = ["bmux.presenter"]\nsearch_paths = '
                               + json.dumps([str(self.args.plugins)]) + '\n' + extra)

    def prepare_deck(self):
        def chunk(kind, value):
            return struct.pack("!I", len(value)) + kind + value + struct.pack("!I", zlib.crc32(kind + value))
        png = (b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack("!IIBBBBB", 8, 8, 8, 6, 0, 0, 0))
               + chunk(b"IDAT", zlib.compress((b"\0" + bytes([240, 32, 64, 255]) * 8) * 8))
               + chunk(b"IEND", b""))
        (self.root / "red.png").write_bytes(png)
        self.hidden_png = png[:-12] + chunk(b"tEXt", b"Comment\0" + HIDDEN.encode()) + png[-12:]
        (self.root / "hidden.png").write_bytes(self.hidden_png)
        self.hidden_resource_id = WEB.hashlib.sha256(self.hidden_png).hexdigest()

        self.deck.write_text(f"""<!-- bmux:slide-id first -->
# {FIRST}

![red square](red.png)

<!-- bmux:block-id counter -->

<!-- bmux:component counter -->

<!-- bmux:notes -->
{SECRET}
<!-- bmux:end-notes -->

<!-- bmux:pause -->
{HIDDEN}

![{HIDDEN}](hidden.png)

---
<!-- bmux:slide-id last -->
# {LAST}

<!-- bmux:notes -->
{SECRET}
<!-- bmux:end-notes -->
""")

    def cli(self, *argv):
        result = subprocess.run([*self.command, *map(str, argv)], env=self.env,
                                stdin=subprocess.DEVNULL, capture_output=True, timeout=30)
        if result.returncode != 0:
            diagnostic = result.stderr.decode(errors="replace")[-4096:]
            diagnostic = re.sub(r"https?://[^\s]+|tls://[^\s]+", "[redacted-url]", diagnostic)
            diagnostic = re.sub(r"[a-fA-F0-9]{32,}", "[redacted-capability]", diagnostic)
            diagnostic = diagnostic.replace(self.credential, "[redacted-credential]")
            raise RuntimeError("owned CLI command failed: " + " ".join(map(str, argv[:2]))
                               + ": " + diagnostic)
        return result

    def spawn(self, argv, mirror=False, env=None):
        process = (Mirror if mirror else WEB.Process)([*self.command, *map(str, argv)], env or self.env)
        self.processes.append(process)
        return process

    def url(self, process):
        def ready():
            require(process.child.poll() is None, "presenter exited before browser URL")
            match = re.search(rb"http://(?:\[[^\]]+\]|[^\s/:]+):\d+/[^\s\x1b]*", process.bytes())
            return match[0].decode() if match else None
        return wait_for("presenter browser URL", ready, 30)

    def browser(self, url, width=1000, height=720, rejected=False, delivery="frame"):
        parts = urllib.parse.urlsplit(url)
        query = dict(urllib.parse.parse_qsl(parts.query))
        query["delivery"] = delivery
        if delivery == "document":
            query["inspect"] = "1"
            query["adapter"] = "/presenter-adapter.js"
        else:
            query.pop("adapter", None)
        url = urllib.parse.urlunsplit(parts._replace(query=urllib.parse.urlencode(query)))
        self.browser_sequence += 1
        profile = self.root / f"browser-{self.browser_sequence}"
        process = WEB.Process([str(self.args.browser), "--headless", "--disable-gpu", "--no-first-run",
            "--no-default-browser-check", "--remote-debugging-address=127.0.0.1",
            "--remote-debugging-port=0", "--user-data-dir=" + str(profile), "about:blank"], self.env)
        self.browser_processes.append(process)
        portfile = profile / "DevToolsActivePort"
        wait_for("Chromium", lambda: portfile.exists() and process.child.poll() is None)
        port = int(portfile.read_text().splitlines()[0])
        opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
        with opener.open(f"http://127.0.0.1:{port}/json", timeout=3) as response:
            page = next(p for p in json.load(response) if p["type"] == "page" and p["url"] == "about:blank")
        client = WEB.CDP(page["webSocketDebuggerUrl"])
        self.clients.append(client)
        events = []
        receive = client._message
        def receive_with_evidence():
            message = receive()
            if message.get("method") in ("Runtime.consoleAPICalled", "Runtime.exceptionThrown",
                    "Network.responseReceived", "Network.loadingFailed", "Log.entryAdded"):
                events.append(message)
                del events[:-200]
            return message
        client._message = receive_with_evidence
        client.call("Runtime.enable")
        client.call("Log.enable")
        client.call("Page.enable")
        client.call("Network.enable")
        client.call("Emulation.setDeviceMetricsOverride", {"width": width, "height": height,
                    "deviceScaleFactor": 1, "mobile": False})
        if delivery == "document":
            client.call("Page.addScriptToEvaluateOnNewDocument", {"source": DOCUMENT_PROBE +
                        "\nwindow.__audiencePixels = () => " + WEB.PIXELS + ";"})
        navigation = client.call("Page.navigate", {"url": url})
        if rejected:
            wait_for("invalid capability rejected by browser", lambda: "Access denied" in
                     (client.evaluate("document.getElementById('status')?.textContent") or ""))
        else:
            try:
                prefix = "Read-only · responsive · revision" if delivery == "document" else "Read-only · revision"
                wait_for("WASM Canvas commit", lambda: (client.evaluate(WEB.PIXELS) or {}).get("status", "").startswith(prefix))
            except RuntimeError:
                diagnostic = {"requested_url": url, "navigation": navigation,
                    "page": client.evaluate("({url:location.href,ready:document.readyState,"
                        "title:document.title,html:document.documentElement.outerHTML})"),
                    "events": events, "browser_output": process.bytes().decode(errors="replace")}
                stem = self.root / f"browser-{self.browser_sequence}-failure"
                stem.with_suffix(".json").write_text(json.dumps(diagnostic, indent=2))
                try:
                    screenshot = client.call("Page.captureScreenshot", {"format": "png"})
                    stem.with_suffix(".png").write_bytes(base64.b64decode(screenshot["data"]))
                except RuntimeError:
                    pass
                status = client.evaluate("document.getElementById('status')?.textContent") or "missing status"
                status = re.sub(r"https?://[^\s]+|[a-fA-F0-9]{32,}", "[redacted]", status)
                raise RuntimeError("WASM Canvas commit failed; profile " + str(self.browser_sequence)
                                   + "; browser status: " + status[:240]) from None
        require(client.evaluate("location.hash === ''"), "bootstrap capability remained in location")
        return client

    def browser_rejected(self, url):
        return self.browser(url, rejected=True)

    def endpoint(self, url):
        parts = urllib.parse.urlsplit(url)
        return "http://" + parts.netloc, urllib.parse.parse_qs(parts.fragment).get("token", [None])[0]

    def frame(self, url, marker=FIRST):
        origin, token = self.endpoint(url)
        def ready():
            status, data = WEB.http(origin, "/snapshot", token)
            require(status in (200, 503), "presenter snapshot rejected")
            if status == 503:
                return None
            value = json.loads(data)
            return value if marker in WEB.text(value) else None
        frame = wait_for("presenter cells: " + marker, ready)
        WEB.unsupported_metadata(frame)
        require(SECRET not in json.dumps(frame), "notes leaked in cells/resources/selection/semantics")
        require(HIDDEN not in json.dumps(frame), "unrevealed text leaked in audience publication")
        return frame

    def commit(self, client, frame):
        return wait_for("matching real Canvas revision", lambda: self.committed(client, frame))

    def committed(self, client, frame):
        pixels = client.evaluate(WEB.PIXELS)
        return pixels if pixels and WEB.rendered_status(pixels["status"], frame) and pixels["ink"] else None

    def privacy_resources(self, client, url):
        # Inspect compact live wire AND every referenced opaque resource in-browser.
        _, token = self.endpoint(url)
        headers = json.dumps({"Authorization": "Bearer " + token} if token else {})
        result = client.evaluate("""(async()=>{
 const hiddenId=%s;
 const headers=%s;
 const m=await import('/bmux_web_renderer.js');
 const response=await fetch('/presentation?version=1',{headers});
 if(!response.ok)throw Error('presentation rejected');
 const b=new Uint8Array(await response.arrayBuffer());
 if(b.length<5||b[0]!==66||b[1]!==77||b[2]!==87||b[3]!==80||b[4]!==3)throw Error('expected BMWP wire v3');
 const refs=JSON.parse(m.CanvasRenderer.binary_resources(b));
 const bodies=[b];
 for(const r of refs){const p=await fetch('/resource/'+encodeURIComponent(r.id),{headers});
 if(!p.ok)throw Error('resource rejected');bodies.push(new Uint8Array(await p.arrayBuffer()));}
 const probe=await fetch('/resource/'+hiddenId,{headers});
 return {hiddenStatus:probe.status,count:refs.length,leaked:bodies.some(x=>new TextDecoder().decode(x).includes('%s')||new TextDecoder().decode(x).includes('%s'))};
})()""" % (json.dumps(self.hidden_resource_id), headers, SECRET, HIDDEN))
        require(result and result["count"] > 0 and not result["leaked"] and result["hiddenStatus"] == 404,
                "image resource privacy/coverage failed")

    def passed(self, label):
        self.cases.append(label)
        print("PASS " + label, flush=True)

    def stop(self, process, url=None):
        process.child.send_signal(signal.SIGINT)
        wait_for("owned presenter shutdown", lambda: process.child.poll() is not None, 10)
        require(process.child.returncode == 0, "presenter shutdown failed")
        if url:
            origin, token = self.endpoint(url)
            try:
                status, _ = WEB.http(origin, "/snapshot", token)
            except (OSError, urllib.error.URLError):
                status = 0
            require(status != 200, "shutdown retained healthy browser feed")

    def headless(self, process):
        require(b"\x1b" not in process.bytes(), "web-only emitted ANSI")
        paths = Path(f"/proc/{process.child.pid}/fd")
        require(paths.is_dir(), "Linux fd inspection unavailable")
        fds = [os.readlink(p) for p in paths.iterdir()]
        require(not any(p == "/dev/tty" or p.startswith("/dev/pts/") for p in fds), "web-only opened TTY")

    def measured_frame(self, client, url):
        metric = client.evaluate("""(()=>{
 const v=document.getElementById('viewport'),s=getComputedStyle(v);
 const p=document.createElement('canvas').getContext('2d');p.font=`${s.fontSize} ${s.fontFamily}`;
 return {cols:Math.floor(v.clientWidth/p.measureText('M').width),rows:Math.floor(v.clientHeight/parseFloat(s.lineHeight))};
})()""")
        def matching():
            frame = self.frame(url)
            area = frame["buffer"]["area"]
            return frame if (area["width"], area["height"]) == (metric["cols"], metric["rows"]) else None
        return wait_for("first browser measured geometry committed", matching)

    def document(self, url, marker=FIRST):
        origin, token = self.endpoint(url)
        last_response = {}
        def ready():
            status, data = WEB.http(origin, "/document?version=1", token)
            last_response.update(status=status, body=data.decode(errors="replace"))
            require(status in (200, 503), "audience document rejected")
            if status == 503:
                return None
            envelope = json.loads(data)
            require(envelope.get("version") == 1 and
                    envelope.get("capability") == "bmux.presenter.audience.v1",
                    "audience document capability/version mismatch")
            content = bytes(envelope["content"]).decode("utf-8")
            if marker not in content:
                return None
            require(all(secret not in content for secret in (SECRET, HIDDEN, PRIVATE_CONFIG)),
                    "private/unrevealed audience document data leaked")
            public = json.loads(content)
            def inspect(value):
                if isinstance(value, dict):
                    require("notes" not in value and value.get("config") is None and
                            not ("kind" in value and value.get("source") is not None),
                            "private audience notes/config/source exported")
                    for child in value.values():
                        inspect(child)
                elif isinstance(value, list):
                    for child in value:
                        inspect(child)
            inspect(public)
            ids = set()
            for resource in envelope["resources"]:
                require(re.fullmatch(r"[0-9a-f]{64}", resource["id"]) and resource["id"] not in ids,
                        "document resource identity invalid/duplicate")
                ids.add(resource["id"])
                code, body = WEB.http(origin, "/resource/" + resource["id"], token)
                require(code == 200 and len(body) == resource["length"] and
                        WEB.hashlib.sha256(body).hexdigest() == resource["id"],
                        "document resource bytes/length/hash mismatch")
                require(not any(secret.encode() in body for secret in (SECRET, HIDDEN, PRIVATE_CONFIG)),
                        "private data leaked through document resource")
            require(WEB.http(origin, "/resource/" + self.hidden_resource_id, token)[0] == 404,
                    "unrevealed resource accessible in document mode")
            return envelope, public
        try:
            return wait_for("audience document: " + marker, ready)
        except RuntimeError:
            (self.root / "document-failure.private.json").write_text(json.dumps(last_response, indent=2))
            for index, process in enumerate(self.processes):
                (self.root / f"document-process-{index}.private.log").write_bytes(process.bytes())
            raise

    def inspection(self, client):
        value = client.evaluate("window.__bmuxDocumentInspection?.() ?? null")
        require(value and value.get("frame"), "committed audience Canvas inspection missing")
        frame = value["frame"]
        require(isinstance(frame.get("partial_metadata"), list) and
                isinstance(frame.get("unsupported_metadata"), list),
                "audience metadata availability not explicit")
        require(not any(secret in json.dumps(value) for secret in (SECRET, HIDDEN, PRIVATE_CONFIG)),
                "private data leaked through committed metadata")
        return value

    def local_scene(self, client):
        value = self.inspection(client)
        # Publication and Canvas revisions may advance without changing this view.
        # Compare all geometry/source/focus metadata, not transport counters.
        frame = dict(value["frame"])
        frame.pop("revision", None)
        return {"generation": value["generation"], "viewport": value["viewport"], "frame": frame}

    def document_commit(self, client, envelope):
        def ready():
            value = client.evaluate("window.__bmuxDocumentInspection?.() ?? null")
            pixels = client.evaluate(WEB.PIXELS)
            if (value and value.get("frame") and pixels and pixels["ink"] and
                    value["generation"] == envelope["generation"] and
                    int(value["publication_revision"]) >= int(envelope["publication_revision"]) and
                    int(value["revision"]) >= int(envelope["revision"])):
                return self.inspection(client), pixels
            return None
        try:
            result = wait_for("real audience document Canvas commit", ready)
        except RuntimeError:
            result = None
            raise
        finally:
            diagnostic = {"expected": {key: envelope[key] for key in
                          ("generation", "revision", "publication_revision")},
                "inspection": client.evaluate("window.__bmuxDocumentInspection?.() ?? null"),
                "pixels": client.evaluate(WEB.PIXELS),
                "dom": client.evaluate("({url:location.href,status:document.getElementById('status')?.textContent,"
                    "viewport:document.getElementById('viewport')?.getBoundingClientRect().toJSON(),"
                    "canvases:Array.from(document.querySelectorAll('canvas'),c=>({id:c.id,width:c.width,"
                    "height:c.height,hidden:c.hidden,rect:c.getBoundingClientRect().toJSON()}))})")}
            client_id = self.clients.index(client) + 1
            stem = self.root / f"document-commit-{client_id}-{envelope['publication_revision']}"
            stem.with_suffix(".private.json").write_text(json.dumps(diagnostic, indent=2))
            if result is None:
                screenshot = client.call("Page.captureScreenshot", {"format": "png"})
                stem.with_suffix(".png").write_bytes(base64.b64decode(screenshot["data"]))
        return result

    def semantic_audience(self):
        # Dedicated source: long prose must wrap locally, code must remain native,
        # and the same image identity appears on both navigable slides.
        source = self.root / "responsive.md"
        prose = " ".join([REFLOW + " local audience layout preserves every word."] * 4)
        tail = " ".join(["AUDIENCE_SCROLL_TAIL_9327 keeps the local view scrollable."] * 60)
        source.write_text(f"""<!-- bmux:slide-id first -->
# {FIRST}

<!-- bmux:block-id responsive-prose -->
{prose}

<!-- bmux:block-id responsive-code -->
```rust
let native_code_9327 = 42;
```

<!-- bmux:block-id responsive-first-image -->
![red square](red.png)

<!-- bmux:block-id counter -->
<!-- bmux:component counter {{"private":"{PRIVATE_CONFIG}"}} -->

<!-- bmux:block-id responsive-prose-tail -->
{tail}

<!-- bmux:notes -->
{SECRET}
<!-- bmux:end-notes -->
<!-- bmux:pause -->
{HIDDEN}
![{HIDDEN}](hidden.png)

---
<!-- bmux:slide-id last -->
# {LAST}

<!-- bmux:block-id responsive-last-image -->
![red square](red.png)

<!-- bmux:block-id responsive-last-prose -->
{prose}
""")
        process = self.spawn(["present", "play", source, "--presenter", "--web-assets", self.args.assets,
                              "--web-port", "0", "--web-auth", "none"], mirror=True,
                             env=dict(self.env, BMUX_PRESENTER_WEB_DELIVERY="document"))
        url = self.url(process)
        envelope, public = self.document(url)
        require(public.get("surfaces"), "fixed custom raster fixture not exported")
        # Surface revisions can advance; exact retained cells/images are checked below.
        narrow = self.browser(url, 440, 1100, delivery="document")
        wide = self.browser(url, 1100, 1100, delivery="document")
        ni, np = self.document_commit(narrow, envelope)
        wi, wp = self.document_commit(wide, envelope)
        require(ni["viewport"] != wi["viewport"] and ni["frame"] != wi["frame"],
                "simultaneous audiences did not compose distinct local layouts")
        require(np["red"] > 0 and wp["red"] > 0, "native image missing in responsive Canvas")
        for pixels in (np, wp):
            require(abs((pixels["maxX"] - pixels["minX"]) -
                        (pixels["maxY"] - pixels["minY"])) <= 2,
                    "native PNG aspect ratio distorted")
            require(abs((pixels["minX"] + pixels["maxX"]) / 2 - pixels["width"] / 2) < 20,
                    "native PNG not centered in locally composed allocation")
        require(np["minY"] > wp["minY"] and np["minX"] != wp["minX"],
                "PNG placement did not follow locally wrapped prose/cell transforms")
        # Selection/semantics are canonical source metadata, not OCR of cell pixels.
        logical = json.dumps(wi["frame"])
        require(REFLOW in logical and "native_code_9327" in logical,
                "native prose/code missing from committed source metadata")
        def prose_geometry(frame):
            texts = frame.get("selection_text", {})
            ids = {key for key, text in texts.items() if REFLOW in text}
            require(len(ids) == 1, "wrap probe must identify exactly one visible source")
            fragments = [fragment for fragment in frame["selection"]["fragments"]
                         if fragment["content_id"] in ids]
            require(fragments, "native prose source-linked geometry missing")
            source = texts[next(iter(ids))].encode("utf-8")
            covered = set()
            spans = {}
            for fragment in fragments:
                bounds = fragment["source_range"]
                covered.update(range(bounds["start"], bounds["end"]))
                area = fragment["area"]
                left, right = spans.get(area["y"], (area["x"], area["x"]))
                spans[area["y"]] = min(left, area["x"]), max(right, area["x"] + area["width"])
            require(all(index in covered for index, byte in enumerate(source) if not chr(byte).isspace()),
                    "wrap probe source clipped or missing visible geometry")
            width = max(right - left for left, right in spans.values())
            return len(spans), width
        nrows, nwidth = prose_geometry(ni["frame"])
        wrows, wwidth = prose_geometry(wi["frame"])
        require(nrows > wrows and nwidth < wwidth,
                "native prose did not rewrap into more narrower visual rows")
        require("selection" in wi["frame"]["partial_metadata"] and
                "semantics" in wi["frame"]["partial_metadata"],
                "embedded raster incorrectly advertises complete logical metadata")
        counts = [c.evaluate("({...window.__audienceProbe.resources})") for c in (narrow, wide)]
        require(all(counts) and all(all(n == 1 for n in count.values()) for count in counts),
                "initial document resources missing or redundantly fetched")
        wide_before = self.local_scene(wide)
        narrow.call("Input.dispatchMouseEvent", {"type": "mouseWheel", "x": 200, "y": 200,
                    "deltaX": 0, "deltaY": 180})
        wait_for("local audience scroll repaint", lambda:
                 (narrow.evaluate(WEB.PIXELS) or {}).get("checksum") != np["checksum"])
        require(self.local_scene(wide) == wide_before and
                wide.evaluate(WEB.PIXELS)["checksum"] == wp["checksum"],
                "one audience scroll rearranged another")
        wide_focus = wide.evaluate("document.activeElement?.id")
        narrow.evaluate("document.activeElement?.blur()")
        narrow.call("Input.dispatchKeyEvent", {"type": "keyDown", "key": "Tab", "code": "Tab",
                    "windowsVirtualKeyCode": 9})
        narrow.call("Input.dispatchKeyEvent", {"type": "keyUp", "key": "Tab", "code": "Tab",
                    "windowsVirtualKeyCode": 9})
        wait_for("local Canvas focus", lambda: narrow.evaluate("document.activeElement?.id") == "bmux")
        require(wide.evaluate("document.activeElement?.id") == wide_focus and
                self.local_scene(wide) == wide_before, "audience focus crossed viewers")
        narrow.call("Emulation.setDeviceMetricsOverride", {"width": 620, "height": 1100,
                    "deviceScaleFactor": 1, "mobile": False})
        wait_for("local audience resize", lambda:
                 (narrow.evaluate("window.__bmuxDocumentInspection?.()") or {}).get("viewport") != ni["viewport"])
        require(self.local_scene(wide) == wide_before, "one audience resize rearranged another")
        unchanged, current = self.document(url)
        require(current["state"] == public["state"] and current["deck"] == public["deck"] and
                current["assets"] == public["assets"] and
                [surface["frame"]["buffer"] for surface in current["surfaces"]] ==
                [surface["frame"]["buffer"] for surface in public["surfaces"]] and
                [surface["frame"]["image_scene"] for surface in current["surfaces"]] ==
                [surface["frame"]["image_scene"] for surface in public["surfaces"]],
                "browser layout mutated shared navigation or fixed producer cells/images")
        require(process.child.poll() is None, "resize stopped native presenter lifecycle")
        for client, count in zip((narrow, wide), counts):
            require(client.evaluate("({...window.__audienceProbe.resources})") == count,
                    "resource refetched on resize/scroll/focus")
        # Invalid JSON, stale source revision, and resource failure must not move
        # either visible pixels or canonical metadata past the last good scene.
        for fault in ("invalid", "stale", "resource"):
            before_scene = self.local_scene(narrow)
            before = self.inspection(narrow)
            pixels = narrow.evaluate(WEB.PIXELS)
            narrow.evaluate("window.__audienceProbe.fault = " + json.dumps(fault) +
                            "; window.__audienceProbe.faults = 0; window.__audienceProbe.failures = [];"
                            " window.__audienceProbe.baseline = null; window.__audienceProbe.candidate = null")
            process.send(b"\r\r")
            latest, _ = self.document(url, LAST)
            wait_for("injected document preparation failure", lambda:
                     narrow.evaluate("window.__audienceProbe.faults") > 0 and
                     narrow.evaluate("window.__audienceProbe.failures.length") > 0)
            baseline = narrow.evaluate("window.__audienceProbe.baseline")
            require(baseline and baseline["inspection"], "fault delivery baseline missing")
            before = baseline["inspection"]
            pixels = baseline["pixels"]
            before_frame = dict(before["frame"])
            before_frame.pop("revision", None)
            before_scene = {"generation": before["generation"], "viewport": before["viewport"],
                            "frame": before_frame}
            after = self.inspection(narrow)
            after_pixels = narrow.evaluate(WEB.PIXELS)
            (self.root / f"fault-{fault}.private.json").write_text(json.dumps({
                "before": before, "after": after, "before_pixels": pixels,
                "after_pixels": after_pixels, "probe": narrow.evaluate("window.__audienceProbe")}, indent=2))
            require(self.local_scene(narrow) == before_scene and
                    after["generation"] == before["generation"] and
                    after["publication_revision"] == before["publication_revision"] and
                    after["revision"] == before["revision"] and
                    after_pixels["checksum"] == pixels["checksum"],
                    "failed document advanced committed cells/images/metadata")
            self.document_commit(wide, latest)
            narrow.evaluate("window.__audienceProbe.fault = null")
            self.document_commit(narrow, latest)
            require(narrow.evaluate(WEB.PIXELS)["red"] > 0,
                    "recovery lost native image")
            process.send(b"\x1b[D\x1b[D")
            first, _ = self.document(url)
            self.document_commit(narrow, first)
            self.document_commit(wide, first)
        for client, count in zip((narrow, wide), counts):
            after = client.evaluate("({...window.__audienceProbe.resources})")
            require(all(after.get(path) == number for path, number in count.items()),
                    "cached image resource refetched on navigation/recovery")
        self.stop(process, url)
        self.passed("document audience: simultaneous local reflow/scroll/focus; shared navigation; "
                    "fixed raster/privacy/partial metadata; cache; invalid/stale/resource recovery")

    def semantic_mermaid(self):
        source = self.root / "responsive-mermaid.md"
        source.write_text(f"""# {FIRST}

<!-- bmux:block-id prepared-diagram -->
```mermaid
flowchart LR
  A[Native Mermaid] --> B[Prepared PNG]
```

<!-- bmux:notes -->
{SECRET}
<!-- bmux:end-notes -->
""")
        process = self.spawn(["present", "play", source, "--web-only", "--web-assets", self.args.assets,
                              "--web-port", "0", "--web-auth", "none"],
                             env=dict(self.env, BMUX_PRESENTER_WEB_DELIVERY="document"))
        url = self.url(process)
        envelope, public = self.document(url)
        blocks = [block for slide in public["deck"]["slides"] for block in slide["blocks"]]
        diagram = next((block for block in blocks if block["id"] == "prepared-diagram"), None)
        require(diagram and diagram["kind"]["type"] == "image" and
                diagram["kind"]["destination"] in public["assets"],
                "loader did not substitute Mermaid with prepared image")
        require(envelope["resources"] and
                all(resource["mime_type"] == "image/png" for resource in envelope["resources"]),
                "Mermaid exported source/SVG rather than prepared PNG")
        narrow = self.browser(url, 440, 600, delivery="document")
        wide = self.browser(url, 1100, 600, delivery="document")
        _, small = self.document_commit(narrow, envelope)
        _, large = self.document_commit(wide, envelope)
        require(small["ink"] > 0 and large["ink"] > 0,
                "prepared Mermaid image not painted in audience Canvas")
        # Count genuinely colored pixels, excluding white text and dark background:
        # this diagram's default node/edge palette must survive the real image path.
        colored = """(()=>{const c=document.getElementById('bmux');
 const d=c.getContext('2d').getImageData(0,0,c.width,c.height).data;let n=0;
 for(let i=0;i<d.length;i+=4)if(Math.max(d[i],d[i+1],d[i+2])-Math.min(d[i],d[i+1],d[i+2])>20&&Math.max(d[i],d[i+1],d[i+2])>100)n++;
 return n;})()"""
        colored_counts = [client.evaluate(colored) for client in (narrow, wide)]
        for index, (client, pixels, count) in enumerate(zip((narrow, wide), (small, large), colored_counts)):
            (self.root / f"mermaid-{index}.private.json").write_text(json.dumps({
                "inspection": self.inspection(client), "pixels": pixels, "colored_pixels": count,
                "current_pixels": client.evaluate(WEB.PIXELS)}, indent=2))
            screenshot = client.call("Page.captureScreenshot", {"format": "png"})
            (self.root / f"mermaid-{index}.png").write_bytes(base64.b64decode(screenshot["data"]))
        require(all(count > 100 for count in colored_counts),
                "Mermaid prepared PNG missing from actual Canvas output")
        self.stop(process, url)
        self.passed("document Mermaid: explicit worker; loader PNG substitution; real Canvas in two widths")

    def play(self, mirror):
        argv = ["present", "play", self.deck, "--presenter", "--web-assets", self.args.assets,
                "--web-port", "0", "--web-auth", "none"]
        if not mirror:
            argv.append("--web-only")
        process = self.spawn(argv, mirror)
        url = self.url(process)
        initial = self.frame(url)
        if mirror:
            wait_for("local presenter notes", lambda: SECRET.encode() in process.bytes())
            require(initial["buffer"]["area"]["width"] == 120 and initial["buffer"]["area"]["height"] == 26,
                    "mirror did not retain local audience geometry (40 rows minus notes/footer)")
        else:
            self.headless(process)
            require((initial["buffer"]["area"]["width"], initial["buffer"]["area"]["height"]) == (80, 24),
                    "web-only bootstrap is not 80x24")
        client = self.browser(url)
        frame = self.frame(url) if mirror else self.measured_frame(client, url)
        pixels = self.commit(client, frame)
        require("Counter: 0" in WEB.text(frame), "linked custom component missing")
        require(WEB.placements(frame) and pixels["red"] > 0, "prepared image missing from scene/Canvas")
        self.privacy_resources(client, url)
        area = frame["buffer"]["area"]
        if not mirror:
            require(area != initial["buffer"]["area"], "first browser did not become viewport authority")
        observer = self.browser(url, 500, 400)
        time.sleep(0.5)
        require(self.frame(url)["buffer"]["area"] == area, "additional browser resized authority")
        self.commit(observer, self.frame(url))
        origin, token = self.endpoint(url)
        status, body = WEB.http(origin, "/view", token, "POST", b"")
        observer_info = json.loads(body)
        require(status == 200 and not observer_info["viewport_allowed"] and observer_info["controller"] is None,
                "additional viewer obtained viewport lease")
        if not mirror:
            wait_for("static headless notice", lambda: b"Static read-only audience view" in process.bytes()
                     and b"navigation is disabled" in process.bytes())
            time.sleep(0.5)
            require(FIRST in WEB.text(self.frame(url)) and LAST not in WEB.text(self.frame(url)),
                    "unscheduled headless play advanced navigation")
            self.headless(process)
            self.stop(process, url)
            self.passed("play web-only static/no navigation/no TTY/ANSI; first-browser geometry; image/custom/privacy/observers/shutdown")
            return
        # End belongs to the scroll view when content overflows. Enter is the
        # existing slide-next binding; first reveal, then advance to last slide.
        process.send(b"\r")
        wait_for("local reveal navigation", lambda: HIDDEN.encode() in process.bytes())
        process.send(b"\r")
        changed = self.frame(url, LAST)
        updated = self.commit(client, changed)
        require(changed["revision"] > frame["revision"] and updated["checksum"] != pixels["checksum"],
                "real play update did not repaint Canvas")
        self.commit(observer, changed)
        wait_for("local slide navigation", lambda: LAST.encode() in process.bytes())
        self.stop(process, url)
        self.passed("play mirror local notes versus audience/navigation; image/custom/privacy/observers/shutdown")

    def host(self, mirror=False):
        subprocess.run(["openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "1",
            "-subj", "/CN=localhost", "-addext", "subjectAltName=DNS:localhost,IP:127.0.0.1",
            "-addext", "basicConstraints=critical,CA:FALSE", "-addext", "keyUsage=critical,digitalSignature,keyEncipherment",
            "-addext", "extendedKeyUsage=serverAuth", "-keyout", str(self.root / "key.pem"),
            "-out", str(self.root / "cert.pem")], env=self.env, check=True, timeout=20,
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        (self.root / "key.pem").chmod(0o600)
        argv = ["present", "host", self.deck, "--listen", "127.0.0.1:0",
            "--cert", self.root / "cert.pem", "--key", self.root / "key.pem",
            "--presenter-credential", self.root / "presenter", "--public-read",
            "--web-assets", self.args.assets, "--web-port", "0", "--web-auth", "token"]
        if not mirror:
            argv.append("--web-only")
        process = self.spawn(argv, mirror=mirror)
        url = self.url(process)
        origin, token = self.endpoint(url)
        require(token, "explicit token authentication absent")
        require(WEB.http(origin, "/snapshot", None)[0] in (401, 403), "unauthenticated snapshot accepted")
        require(WEB.http(origin, "/snapshot", "invalid")[0] in (401, 403), "invalid bearer accepted")
        require(WEB.http(origin, "/presentation?version=1", None)[0] in (401, 403), "unauthenticated wire accepted")
        require(WEB.http(origin, "/resource/" + "0" * 64, None)[0] in (401, 403), "unauthenticated resource accepted")
        rejected = self.browser_rejected(origin + "/#token=invalid")
        # A fragment-only Page.navigate does not reload bootstrap JavaScript.
        rejected.call("Page.navigate", {"url": "about:blank"})
        wait_for("rejected page unload", lambda: rejected.evaluate("location.href") == "about:blank")
        frame_url = urllib.parse.urlunsplit(urllib.parse.urlsplit(url)._replace(query="delivery=frame"))
        rejected.call("Page.navigate", {"url": frame_url})
        wait_for("auth rejection browser recovery", lambda: (rejected.evaluate(WEB.PIXELS) or {}).get("status", "").startswith("Read-only · revision"))
        client = self.browser(url)
        before = self.frame(url)
        pixels = self.commit(client, before)
        self.privacy_resources(client, url)
        require("Counter: 0" in WEB.text(before) and pixels["red"] > 0, "host image/custom Canvas missing")
        def tls_ready():
            match = re.search(rb"tls://127\.0\.0\.1:(\d+)", process.bytes())
            return int(match[1]) if match else None
        port = wait_for("real host TLS control", tls_ready)
        wire = CONTROL.Wire(("127.0.0.1", port), self.root / "cert.pem", self.credential, "Shared")
        try:
            state = wire.snapshot()
            component = next((s for s in state["surfaces"] if s["surface_id"] == "counter"), None)
            require(component, "host custom surface missing")
            effect = wire.submit(wire.auth["next_action_id"], state, {"DemoInput": {
                "surface_id": "counter", "surface_revision": component["revision"],
                "x": 0, "y": 0, "input": "+"}})
            require("Acknowledged" in effect, "authorized custom input failed")
            custom = self.frame(url, "Counter: 1")
            require(self.commit(client, custom)["checksum"] != pixels["checksum"], "custom update did not repaint")
            state = wire.snapshot()
            ack = wire.submit(effect["Acknowledged"]["action_id"] + 1, state, {"Navigate": "Last"})
            require("Acknowledged" in ack, "host navigation failed")
            after = self.frame(url, LAST)
            require(self.commit(client, after)["checksum"] != pixels["checksum"], "host navigation did not repaint")
            # Force a real transport loss, mutate host, then require latest recovery.
            client.call("Network.setBlockedURLs", {"urls": [origin + "/presentation*"]})
            client.call("Network.emulateNetworkConditions", {"offline": True, "latency": 0,
                        "downloadThroughput": -1, "uploadThroughput": -1})
            state = wire.snapshot()
            require("Acknowledged" in wire.submit(ack["Acknowledged"]["action_id"] + 1, state,
                    {"Navigate": "First"}), "offline host navigation failed")
            wait_for("observed reconnecting", lambda: "reconnecting" in
                     (client.evaluate(WEB.PIXELS) or {}).get("status", ""), 40)
            latest = self.frame(url)
            client.call("Network.setBlockedURLs", {"urls": []})
            client.call("Network.emulateNetworkConditions", {"offline": False, "latency": 0,
                        "downloadThroughput": -1, "uploadThroughput": -1})
            self.commit(client, latest)
            require(WEB.http(origin, "/view", token, "POST", b"", request_origin="http://invalid.example")[0] == 403,
                    "invalid Origin accepted")
            self.commit(client, self.frame(url))
            if mirror:
                wait_for("host mirror local update", lambda: LAST.encode() in process.bytes())
            else:
                self.headless(process)
        finally:
            wire.close()
        self.stop(process, url)
        self.passed("host " + ("mirror" if mirror else "web-only") + " TLS navigation/custom update; resource privacy; token/Origin recovery; reconnect; shutdown")

    def configuration(self):
        # Off must remain local even when all shared network settings/assets exist.
        self.write_config('[web]\nport = 0\nauth = "none"\n[presenter.web]\nmode = "off"\nassets = '
                          + json.dumps(str(self.args.assets)) + '\n')
        env = dict(self.env, BMUX_WEB_PORT="0", BMUX_WEB_AUTH="none", BMUX_WEB_ASSETS=str(self.args.assets))
        process = self.spawn(["present", "play", self.deck], mirror=True, env=env)
        wait_for("off-mode local rendering", lambda: FIRST.encode() in process.bytes())
        time.sleep(0.5)
        require(b"http://" not in process.bytes(), "shared web settings enabled presenter implicitly")
        self.stop(process)
        # Environment mode beats config; CLI network auth beats environment beats config.
        env.update(BMUX_PRESENTER_WEB_MODE="only", BMUX_WEB_AUTH="token")
        process = self.spawn(["present", "play", self.deck, "--web-auth", "none"], env=env)
        url = self.url(process)
        require(self.endpoint(url)[1] is None, "CLI auth did not override environment token")
        self.browser(url)
        self.frame(url)
        self.headless(process)
        self.stop(process, url)
        process = self.spawn(["present", "play", self.deck], env=env)
        url = self.url(process)
        require(self.endpoint(url)[1], "environment token did not override config none")
        self.browser(url)
        self.frame(url)
        self.stop(process, url)
        self.write_config('[web]\nport = 0\nauth = "none"\n[presenter.web]\nmode = "only"\nassets = '
                          + json.dumps(str(self.args.assets)) + '\n')
        process = self.spawn(["present", "play", self.deck])
        url = self.url(process)
        self.browser(url)
        self.frame(url)
        self.headless(process)
        self.stop(process, url)
        # Explicit assets beat an env-only mode and invalid lower-priority assets.
        override_env = dict(self.env, BMUX_PRESENTER_WEB_MODE="only", BMUX_WEB_ASSETS="/missing/presenter-assets")
        process = self.spawn(["present", "play", self.deck, "--web-assets", self.args.assets,
                              "--web-port", "0", "--web-auth", "none"], mirror=True, env=override_env)
        url = self.url(process)
        wait_for("explicit assets enables local mirror", lambda: FIRST.encode() in process.bytes())
        self.browser(url)
        self.frame(url)
        self.stop(process, url)
        # Environment network fields and assets beat valid configured alternatives.
        self.write_config('[web]\nport = 0\nauth = "none"\nhost = "127.0.0.2"\n'
                          'public_host = "127.0.0.2"\nallowed_hosts = ["config.invalid"]\n'
                          '[presenter.web]\nmode = "only"\nassets = "/missing/config-assets"\n')
        with socket.socket() as reservation:
            reservation.bind(("127.0.0.1", 0))
            expected_port = reservation.getsockname()[1]
        env = dict(self.env, BMUX_WEB_HOST="127.0.0.1", BMUX_WEB_PUBLIC_HOST="127.0.0.1",
                   BMUX_WEB_ALLOWED_HOSTS=f"127.0.0.1:{expected_port}", BMUX_WEB_PORT=str(expected_port),
                   BMUX_WEB_AUTH="token", BMUX_WEB_ASSETS=str(self.args.assets))
        process = self.spawn(["present", "play", self.deck], env=env)
        url = self.url(process)
        parts = urllib.parse.urlsplit(url)
        require(parts.hostname == "127.0.0.1" and parts.port == expected_port and self.endpoint(url)[1],
                "env network precedence mismatch")
        self.browser(url)
        self.frame(url)
        self.stop(process, url)
        # Every CLI network field beats the environment (including invalid values).
        env.update(BMUX_WEB_HOST="invalid", BMUX_WEB_PUBLIC_HOST="invalid", BMUX_WEB_ALLOWED_HOSTS="env.invalid",
                   BMUX_WEB_PORT="invalid", BMUX_WEB_AUTH="invalid", BMUX_WEB_ASSETS="/missing/env-assets")
        with socket.socket() as reservation:
            reservation.bind(("127.0.0.1", 0))
            cli_port = reservation.getsockname()[1]
        process = self.spawn(["present", "play", self.deck, "--web-only", "--web-assets", self.args.assets,
                              "--web-host", "127.0.0.1", "--web-public-host", "127.0.0.1",
                              "--web-allowed-hosts", f"127.0.0.1:{cli_port}", "--web-port", str(cli_port), "--web-auth", "none"], env=env)
        url = self.url(process)
        require(self.endpoint(url)[1] is None, "CLI network auth precedence mismatch")
        self.browser(url)
        self.frame(url)
        self.stop(process, url)
        self.write_config()
        for operation in ("check", "join"):
            result = subprocess.run([*self.command, "present", operation, str(self.deck),
                                     "--web-assets", str(self.args.assets)], env=self.env,
                                    stdin=subprocess.DEVNULL, capture_output=True, timeout=15)
            require(result.returncode != 0, operation + " unexpectedly accepts web flags")
        self.passed("shared settings do not enable web; presenter config/environment opt-in; CLI > env > config auth; check/join reject web")

    def network(self, address, ipv6=False):
        host = "::" if ipv6 else "0.0.0.0"
        for auth in ("none", "token"):
            with socket.socket(socket.AF_INET6 if ipv6 else socket.AF_INET) as reservation:
                reservation.bind((address, 0))
                port = reservation.getsockname()[1]
            authority = f"[{address}]:{port}" if ipv6 else f"{address}:{port}"
            process = self.spawn(["present", "play", self.deck, "--web-assets", self.args.assets,
                "--web-only", "--web-port", str(port), "--web-host", host, "--web-public-host", address,
                "--web-allowed-hosts", authority, "--web-auth", auth])
            url = self.url(process)
            require(urllib.parse.urlsplit(url).hostname == address, "public-host advertisement mismatch")
            client = self.browser(url)
            self.commit(client, self.frame(url))
            origin, token = self.endpoint(url)
            require(bool(token) == (auth == "token"), "network auth mismatch")
            require(WEB.http(origin, "/view", token, "POST", b"", request_origin="http://invalid.example")[0] == 403,
                    "network host Origin rejection absent")
            self.commit(client, self.frame(url))
            self.stop(process, url)
        self.passed(("IPv6" if ipv6 else "LAN") + " explicit bind/public-host/allowed-hosts; none/token; rejection recovery")

    def close_browsers(self):
        for client in self.clients:
            client.close()
        self.clients.clear()
        for process in self.browser_processes:
            process.stop()
        self.browser_processes.clear()

    def run_case(self, function, *args, **kwargs):
        try:
            function(*args, **kwargs)
        finally:
            self.close_browsers()

    def run(self):
        server = self.spawn(["server", "start", "--foreground-internal", "--no-pane-shell-integration"])
        def ready():
            require(server.child.poll() is None, "owned foreground server died")
            result = subprocess.run([*self.command, "server", "status", "--json"], env=self.env,
                                    stdin=subprocess.DEVNULL, capture_output=True, timeout=5)
            return result.returncode == 0 and json.loads(result.stdout).get("running")
        wait_for("isolated owned server", ready, 30)
        self.cli("present", "check", self.deck)
        self.run_case(self.play, False)
        self.run_case(self.play, True)
        self.run_case(self.host)
        self.run_case(self.host, mirror=True)
        self.run_case(self.semantic_audience)
        self.run_case(self.semantic_mermaid)
        self.run_case(self.configuration)
        self.run_case(self.network, self.args.lan_address)
        if socket.has_ipv6:
            # A kernel with IPv6 disabled is an explicit capability result, not a passed assertion.
            try:
                probe = None
                probe = socket.socket(socket.AF_INET6)
                probe.bind(("::1", 0))
            except OSError:
                self.ipv6 = "unavailable: kernel loopback bind rejected"
            else:
                self.run_case(self.network, "::1", True)
                self.ipv6 = "PASS"
            finally:
                if probe is not None:
                    probe.close()
        else:
            self.ipv6 = "unavailable: Python socket.has_ipv6 is false"

    def cleanup(self):
        failures = []
        for process in self.processes:
            if isinstance(process, Mirror):
                # Private evidence; never print raw terminal notes/capabilities.
                (self.root / f"mirror-{process.child.pid}.pty").write_bytes(process.bytes())
        for client in self.clients:
            try:
                client.close()
            except OSError:
                failures.append("CDP close failed")
        for process in reversed(self.browser_processes + self.processes):
            try:
                process.stop()
            except (OSError, subprocess.TimeoutExpired):
                failures.append("owned child cleanup failed")
        (self.root / "report.json").write_text(json.dumps({"identity": self.identity,
            "passed_cases": self.cases, "failure": self.failed,
            "status": "FAIL" if self.failed or failures else "PASS",
            "ipv6": getattr(self, "ipv6", "not reached"),
            "cleanup_failures": failures}, indent=2))
        require(not failures, "owned cleanup incomplete; inspect retained evidence")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("binary", "plugins", "assets", "browser"):
        parser.add_argument("--" + name, type=lambda s: Path(s).resolve(strict=True), required=True)
    parser.add_argument("--state-parent", type=Path, default=Path("/tmp"))
    parser.add_argument("--lan-address", required=True,
                        help="owned machine's non-loopback IPv4 address; required real LAN bind/browser test")
    args = parser.parse_args()
    require(args.binary.is_file() and os.access(args.binary, os.X_OK), "explicit bmux binary not executable")
    require(args.browser.is_file() and os.access(args.browser, os.X_OK), "explicit Chromium not executable")
    require(args.plugins.is_dir() and args.assets.is_dir(), "explicit staged directories required")
    for name in (*WEB.ASSETS, "presenter-adapter.js", "bmux_presenter.js", "bmux_presenter_bg.wasm"):
        require((args.assets / name).is_file(), "missing coordinator-built WASM asset: " + name)
    import ipaddress
    address = ipaddress.IPv4Address(args.lan_address)
    require(not address.is_loopback and not address.is_unspecified, "LAN address must be non-loopback")
    suite = Acceptance(args)
    try:
        suite.run()
    except Exception as error:
        suite.failed = type(error).__name__ + (": " + str(error) if isinstance(error, (RuntimeError, AssertionError)) else "")
        raise
    finally:
        suite.cleanup()


if __name__ == "__main__":
    try:
        main()
    except Exception as error:
        # Do not print URLs, credentials, CDP payloads, or subprocess output.
        print("FAIL presenter web acceptance: " + type(error).__name__
              + (": " + str(error) if isinstance(error, (RuntimeError, AssertionError)) else ""), file=sys.stderr)
        sys.exit(1)
