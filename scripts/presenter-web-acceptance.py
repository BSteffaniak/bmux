#!/usr/bin/env python3
"""Real presenter browser acceptance. No build/install; all assertions mandatory.

Pending integration contract: present play/host --web-assets PATH [--web-only],
shared --web-{port,auth,host,public-host,allowed-hosts}, [presenter.web] mode/assets.
Run only after coordinator grants the browser/runtime resource lock. Evidence is
private, retained, and capability-safe in console output. Linux /proc required.
"""
import argparse
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
                        BMUX_RUNTIME_NAME="default", BMUX_TARGET="local", TERM="xterm-256color")
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
                         "assets": {name: WEB.digest(args.assets / name) for name in WEB.ASSETS},
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

    def browser(self, url, width=1000, height=720, rejected=False):
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
        client.call("Page.enable")
        client.call("Network.enable")
        client.call("Emulation.setDeviceMetricsOverride", {"width": width, "height": height,
                    "deviceScaleFactor": 1, "mobile": False})
        client.call("Page.navigate", {"url": url})
        if rejected:
            wait_for("invalid capability rejected by browser", lambda: "Access denied" in
                     (client.evaluate("document.getElementById('status')?.textContent") or ""))
        else:
            try:
                wait_for("WASM Canvas commit", lambda: (client.evaluate(WEB.PIXELS) or {}).get("status", "").startswith("Read-only · revision"))
            except RuntimeError:
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
        rejected.call("Page.navigate", {"url": url})
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
    for name in WEB.ASSETS:
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
