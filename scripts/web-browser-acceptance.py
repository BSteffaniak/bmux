#!/usr/bin/env python3
"""Part I real-browser acceptance, without builds or user-runtime access.

Run smoke-web-browser.sh --help. Requires a prebuilt bmux, built WASM assets,
Chromium, and JSON emitted by the existing web-renderer snapshot example.
All cases are mandatory; missing dependencies and failed assertions exit 1.
No browser terminal input, DOM/WebGL backend, or responsive policy is tested.
"""
import argparse
import base64
import copy
import hashlib
import http.server as http_server
import importlib.util
import json
import os
import pathlib
import re
import select
import shutil
import signal
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.parse
import urllib.request

sys.dont_write_bytecode = True  # Keep runtime imports out of the shared checkout.
SPEC = importlib.util.spec_from_file_location(
    "web_browser_cdp", pathlib.Path(__file__).with_name("web-browser-cdp.py"))
CDP_MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(CDP_MODULE)
CDP = CDP_MODULE.CDP
ASSETS = {"index.html": "text/html", "style.css": "text/css", "bootstrap.js": "text/javascript",
          "bmux_web_renderer.js": "text/javascript",
          "bmux_web_renderer_bg.wasm": "application/wasm"}
LIMIT = 16 * 1024 * 1024


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def digest(path):
    with open(path, "rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def wait_for(label, predicate, seconds=15):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        value = predicate()
        if value:
            return value
        time.sleep(0.05)
    raise RuntimeError("timeout: " + label)


class Process:
    """Drain bounded output in memory; never persist/print secret-bearing logs."""
    def __init__(self, argv, env):
        self.child = subprocess.Popen(argv, env=env, stdin=subprocess.DEVNULL,
                                      stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                      start_new_session=True)
        self.output = [bytearray(), bytearray()]
        self.lock = threading.Lock()
        self.threads = []
        for index, pipe in enumerate((self.child.stdout, self.child.stderr)):
            thread = threading.Thread(target=self.drain, args=(index, pipe), daemon=True)
            thread.start()
            self.threads.append(thread)

    def drain(self, index, pipe):
        while True:
            data = pipe.read1(65536)
            if not data:
                return
            with self.lock:
                self.output[index].extend(data)
                del self.output[index][:-1024 * 1024]

    def bytes(self):
        with self.lock:
            return b"\n".join(bytes(value) for value in self.output)

    def stop(self):
        if self.child.poll() is None:
            os.killpg(self.child.pid, signal.SIGTERM)
            try:
                self.child.wait(timeout=5)
            except subprocess.TimeoutExpired:
                os.killpg(self.child.pid, signal.SIGKILL)
                self.child.wait(timeout=5)
        for thread in self.threads:
            thread.join(timeout=1)
        self.child.stdout.close()
        self.child.stderr.close()


def producer(root):
    """Owned real PTY producer. FIFO control is not browser terminal input."""
    import fcntl
    import struct
    import termios
    root = pathlib.Path(root)
    identity = {"pid": os.getpid(), "start": pathlib.Path(
        f"/proc/{os.getpid()}/stat").read_text().rsplit(")", 1)[1].split()[19]}
    (root / "producer.json").write_text(json.dumps(identity))
    fifo = os.open(root / "commands", os.O_RDWR)
    rgba = base64.b64encode(bytes([240, 32, 64, 255]) * 4).decode()

    def emit(value):
        sys.stdout.write(value)
        sys.stdout.flush()

    def place(row, col):
        emit(f"\x1b[{row};{col}H\x1b_Ga=p,i=77,p=77,c=12,r=6,C=1,q=2;\x1b\\")

    emit("\x1b[2J\x1b[HWEB_READY")
    with os.fdopen(fifo) as stream:
        for line in stream:
            command = line.strip()
            if command in ("create", "move", "clip", "remove"):
                (root / "producer-last-command").write_text(command)
            if command.startswith("mark "):
                emit("\x1b[1;1H" + command[5:])
            elif command == "sync-begin":
                emit("\x1b[?2026h\x1b[1;1HWEB_ATOMIC_UPDATE")
            elif command == "sync-end":
                emit("\x1b[?2026l")
            elif command == "create":
                emit(f"\x1b_Ga=t,f=32,s=2,v=2,i=77,q=2;{rgba}\x1b\\")
                place(5, 5)
            elif command == "move":
                emit("\x1b_Ga=d,d=p,i=77,p=77,q=2;\x1b\\")
                place(9, 15)
            elif command == "clip":
                rows, cols, _, _ = struct.unpack("HHHH", fcntl.ioctl(
                    sys.stdout.fileno(), termios.TIOCGWINSZ, bytes(8)))
                emit("\x1b_Ga=d,d=p,i=77,p=77,q=2;\x1b\\")
                place(max(2, rows - 2), max(2, cols - 2))
            elif command == "remove":
                emit("\x1b_Ga=d,d=I,i=77,q=2;\x1b\\")
            elif command == "exit":
                return
            else:
                raise RuntimeError("unknown fixture command")


def http(origin, path, token, method="GET", body=None, controller=None,
         request_origin=None):
    headers = {"Authorization": "Bearer " + token,
               "Origin": request_origin or origin}
    if controller:
        headers["X-Bmux-Controller"] = controller
    if isinstance(body, dict):
        body = json.dumps(body).encode()
        headers["Content-Type"] = "application/json"
    request = urllib.request.Request(origin + path, data=body,
                                     headers=headers, method=method)
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    try:
        response = opener.open(request, timeout=3)
    except urllib.error.HTTPError as error:
        response = error
    with response:
        data = response.read(LIMIT + 1)
        require(len(data) <= LIMIT, "HTTP response exceeds bound")
        return response.status, data


def unsupported_metadata(frame):
    require(frame.get("version") == 2, "browser snapshot must use wire v2")
    kinds = frame.get("unsupported_metadata")
    require(isinstance(kinds, list) and len(kinds) <= 2 and
            all(isinstance(kind, str) and kind in ("selection", "semantics") for kind in kinds) and
            len(set(kinds)) == len(kinds), "invalid or missing metadata availability enum list")
    surfaces = frame.get("surfaces", [])
    for surface in surfaces:
        require(type(surface.get("selection_available")) is bool and
                type(surface.get("semantics_available")) is bool,
                "surface metadata availability flags missing")
    require(type(frame.get("selection_text_available")) is bool and
            isinstance(frame.get("selection_text"), dict), "selection source text contract missing")
    if frame["selection_text_available"]:
        for fragment in frame["selection"]["fragments"]:
            content = frame["selection_text"].get(fragment["content_id"])
            require(isinstance(content, str), "selection fragment has no matching source text")
            raw = content.encode("utf-8")
            bounds = fragment["source_range"]
            require(0 <= bounds["start"] <= bounds["end"] <= len(raw),
                    "selection source range outside same-id text")
            try:
                raw[:bounds["start"]].decode("utf-8")
                raw[:bounds["end"]].decode("utf-8")
            except UnicodeDecodeError as error:
                raise RuntimeError("selection source range splits UTF-8") from error
    return kinds


def partial_metadata(frame):
    kinds = unsupported_metadata(frame)
    return [kind for kind in ("selection", "semantics") if kind not in kinds and
            any(not surface[kind + "_available"] for surface in frame["surfaces"])]


def rendered_status(status, frame):
    kinds = unsupported_metadata(frame)
    enum_list = r"((?:selection|semantics)(?:, (?:selection|semantics))?)"
    matched = re.fullmatch(
        r"Read-only · revision ([1-9][0-9]{0,19})"
        + r"(?: · Unavailable metadata: " + enum_list + r")?"
        + r"(?: · Partial metadata: " + enum_list + r")?"
        + r"( · Selection source text unavailable)?", status or "")
    if not matched or int(matched[1]) < frame["revision"]:
        return False
    require(int(matched[1]) <= 18446744073709551615, "browser status revision exceeds u64")
    displayed = matched[2].split(", ") if matched[2] else []
    partial = matched[3].split(", ") if matched[3] else []
    require(displayed == kinds and partial == partial_metadata(frame) and
            bool(matched[4]) == (not frame["selection_text_available"]),
            "browser metadata availability status disagrees with snapshot")
    return True


def placements(frame):
    return list(frame["image_scene"]["active"].values())


def text(frame):
    return "".join(cell["symbol"] for cell in frame["buffer"]["cells"])


# Read Canvas output rather than trusting status, dimensions, or snapshot alone.
PIXELS = """(() => {
 const c=document.getElementById('bmux'), ctx=c?.getContext('2d');
 if (!ctx || !c.width || !c.height) return null;
 const d=ctx.getImageData(0,0,c.width,c.height).data;
 let ink=0, red=0, checksum=2166136261, minX=c.width, minY=c.height, maxX=-1, maxY=-1;
 for(let p=0;p<d.length;p+=4) {
   checksum=Math.imul(checksum ^ d[p] ^ (d[p+1]<<8) ^ (d[p+2]<<16) ^ (d[p+3]<<24),16777619);
   if(d[p]+d[p+1]+d[p+2]>180 && d[p+3]) ink++;
   if(Math.abs(d[p]-240)<4 && Math.abs(d[p+1]-32)<4 && Math.abs(d[p+2]-64)<4 && d[p+3]>250) {
     red++; const i=p/4,x=i%c.width,y=Math.floor(i/c.width);
     minX=Math.min(minX,x);minY=Math.min(minY,y);maxX=Math.max(maxX,x);maxY=Math.max(maxY,y);
   }
 }
 return {width:c.width,height:c.height,ink,red,checksum:checksum>>>0,minX,minY,maxX,maxY,dpr:devicePixelRatio,
 status:document.getElementById('status')?.textContent,hash:location.hash};
})()"""


class Harness:
    def __init__(self, args):
        self.args = args
        self.root = pathlib.Path(tempfile.mkdtemp(prefix="bmux-browser-"))
        os.chmod(self.root, 0o700)
        self.processes = []
        self.client = None
        self.producer_fd = None
        self.fixture_server = None
        self.report = {"cases": [], "artifacts": {"binary": {
            "path": str(args.binary), "sha256": digest(args.binary)},
            "browser": {"path": str(args.browser), "sha256": digest(args.browser)},
            "snapshot": {"path": str(args.standalone_snapshot),
                         "sha256": digest(args.standalone_snapshot)},
            "canvas_conformance": {"path": str(args.canvas_conformance),
                                   "sha256": digest(args.canvas_conformance)},
            "assets": {name: digest(args.assets / name) for name in ASSETS}}}
        self.env = {key: value for key, value in os.environ.items()
                    if not key.startswith(("BMUX_", "XDG_")) and key not in
                    ("HOME", "DBUS_SESSION_BUS_ADDRESS", "DISPLAY", "WAYLAND_DISPLAY")}
        for name in ("config", "runtime", "data", "state", "logs", "home", "tmp", "cache"):
            (self.root / name).mkdir(mode=0o700)
        for key, name in (("BMUX_CONFIG_DIR", "config"), ("BMUX_RUNTIME_DIR", "runtime"),
                          ("BMUX_DATA_DIR", "data"), ("BMUX_STATE_DIR", "state"),
                          ("BMUX_LOG_DIR", "logs"), ("HOME", "home"), ("TMPDIR", "tmp"),
                          ("XDG_CONFIG_HOME", "config"), ("XDG_RUNTIME_DIR", "runtime"),
                          ("XDG_DATA_HOME", "data"), ("XDG_STATE_HOME", "state"),
                          ("XDG_CACHE_HOME", "cache")):
            self.env[key] = str(self.root / name)
        self.config = self.root / "config/bmux.toml"
        self.env.update(BMUX_CONFIG=str(self.config), BMUX_NO_BASE_CONFIG="1",
                        BMUX_TARGET="local", TERM="xterm-256color")
        os.mkfifo(self.root / "commands", 0o600)
        script = self.root / "pane-producer"
        script.write_text(f"#!{sys.executable}\nimport runpy,sys\n"
                          f"sys.argv=[{str(pathlib.Path(__file__).resolve())!r},'--producer',{str(self.root)!r}]\n"
                          f"runpy.run_path({str(pathlib.Path(__file__).resolve())!r},run_name='__main__')\n")
        script.chmod(0o700)
        self.config.write_text('[general]\ndefault_shell = ' + json.dumps(str(script)) + '\n'
            '[keybindings]\ninitial_mode = "normal"\n'
            '[plugins]\nenabled = ["bmux.sessions", "bmux.contexts", "bmux.clients", '
            '"bmux.permissions", "bmux.pane_runtime", "bmux.tabs", "bmux.tab_bar", "bmux.sidebar"]\n')
        self.report["artifacts"]["isolated_config_sha256"] = digest(self.config)
        self.report["isolation"] = {"foreground_daemon": True, "stdin": "null",
            "fresh_home_config_runtime_state": True, "ephemeral_loopback_ports": True}

    def argv(self, *args):
        return [str(self.args.binary), "--config", str(self.config), *args]

    def spawn(self, *args):
        process = Process(self.argv(*args), self.env)
        self.processes.append(process)
        return process

    def command(self, *args, check=True):
        result = subprocess.run(self.argv(*args), env=self.env, stdin=subprocess.DEVNULL,
                                capture_output=True, timeout=15)
        require(not check or result.returncode == 0, "bmux command failed: " + args[0])
        return result

    def passed(self, label, **facts):
        self.report["cases"].append({"case": label, "status": "PASS", **facts})
        print("PASS: " + label, flush=True)

    def fixture_command(self, value):
        fd = os.open(self.root / "commands", os.O_WRONLY | os.O_NONBLOCK)
        try:
            os.write(fd, (value + "\n").encode())
        finally:
            os.close(fd)

    def snapshot(self):
        status, data = http(self.origin, "/snapshot", self.token)
        if status == 503:
            return None
        require(status == 200, "latest snapshot request rejected")
        frame = json.loads(data)
        self.report["latest_snapshot"] = {"revision": frame.get("revision"),
            "area": frame.get("buffer", {}).get("area"),
            "surface_owners": [s.get("owner_plugin_id") for s in frame.get("surfaces", [])],
            "producer_started": (self.root / "producer.json").exists(),
            "producer_last_command": (self.root / "producer-last-command").read_text()
                if (self.root / "producer-last-command").exists() else None,
            "image_count": len(placements(frame)),
            "graphic_placeholder": "image" in text(frame).lower(),
            "ready_marker": "WEB_READY" in text(frame),
            "python_traceback": "Traceback" in text(frame),
            "python_module_error": "ModuleNotFoundError" in text(frame)}
        require(frame["generation"] and frame["buffer"]["cells"], "incomplete snapshot")
        unsupported_metadata(frame)
        return frame

    def frame_when(self, label, predicate):
        def attempt():
            frame = self.snapshot()
            return frame if frame and predicate(frame) else None
        return wait_for(label, attempt)

    def pixels(self):
        return self.client.evaluate(PIXELS)

    def rendered(self, frame, image=None):
        def attempt():
            pixels = self.pixels()
            if not pixels:
                return None
            if not rendered_status(pixels["status"], frame):
                return None
            # Latest-only feeds may skip intermediate publications. Require at
            # least the target revision, not delivery of every native snapshot.
            if image is not None and bool(pixels["red"]) != image:
                return None
            return pixels
        return wait_for("Canvas output commit", attempt)

    def geometry(self, frame):
        metric = self.client.evaluate("""(() => {
 const v=document.getElementById('viewport'),s=getComputedStyle(v);
 const p=document.createElement('canvas').getContext('2d');p.font=`${s.fontSize} ${s.fontFamily}`;
 return {cw:p.measureText('M').width,ch:parseFloat(s.lineHeight),w:v.clientWidth,h:v.clientHeight,dpr:devicePixelRatio};
})()""")
        pixels = self.rendered(frame)
        area = frame["buffer"]["area"]
        require(area["width"] == int(metric["w"] / metric["cw"]) and
                area["height"] == int(metric["h"] / metric["ch"]),
                "browser measured viewport did not reach retained composition")
        require(abs(pixels["width"] - area["width"] * metric["cw"] * metric["dpr"]) <= 1 and
                abs(pixels["height"] - area["height"] * metric["ch"] * metric["dpr"]) <= 1,
                "Canvas backing pixels do not match measured cells/DPR")
        return pixels

    def image_pixels(self, frame):
        items = placements(frame)
        require(len(items) == 1, "owned image placement missing or duplicated")
        image = items[0]
        require("Pixels" in image["payload"] or "Png" in image["payload"],
                "snapshot image has no decoded resource")
        pixels = self.rendered(frame, True)
        area = frame["buffer"]["area"]
        dest, clip = image["destination"], image["clip"]
        x0, y0 = max(dest["x"], clip["x"], area["x"]), max(dest["y"], clip["y"], area["y"])
        x1 = min(dest["x"] + dest["width"], clip["x"] + clip["width"], area["x"] + area["width"])
        y1 = min(dest["y"] + dest["height"], clip["y"] + clip["height"], area["y"] + area["height"])
        cw, ch = pixels["width"] / area["width"], pixels["height"] / area["height"]
        for actual, expected in ((pixels["minX"], (x0-area["x"])*cw),
                                 (pixels["minY"], (y0-area["y"])*ch),
                                 (pixels["maxX"]+1, (x1-area["x"])*cw),
                                 (pixels["maxY"]+1, (y1-area["y"])*ch)):
            require(abs(actual-expected) <= 2, "image pixels disagree with retained destination/clip")
        return image, pixels

    def bootstrap(self, process):
        def attempt():
            require(process.child.poll() is None, "attach exited before bootstrap")
            match = re.search(rb"http://127\.0\.0\.1:\d+/[^\s\x1b]*token=[^\s\x1b]+", process.bytes())
            return match[0].decode() if match else None
        url = wait_for("attach bootstrap", attempt)
        parts = urllib.parse.urlsplit(url)
        self.origin = "http://" + parts.netloc
        self.token = urllib.parse.parse_qs(parts.fragment)["token"][0]
        return url

    def run(self):
        server = self.spawn("server", "start", "--foreground-internal", "--no-pane-shell-integration")
        def ready():
            require(server.child.poll() is None, "owned foreground daemon exited")
            result = self.command("server", "status", "--json", check=False)
            return result.returncode == 0 and json.loads(result.stdout).get("running")
        wait_for("isolated foreground daemon", ready, 25)
        self.command("new-session", "browser-acceptance")
        sessions = json.loads(self.command("list-sessions", "--json").stdout)
        sessions = sessions if isinstance(sessions, list) else sessions["sessions"]
        require(len(sessions) == 1, "isolated runtime must contain exactly one owned session")
        session = sessions[0]["id"]
        attach = self.spawn("attach", session, "--web-assets", str(self.args.assets), "--web-only")
        url = self.bootstrap(attach)
        initial = self.frame_when("live producer ready", lambda f: "WEB_READY" in text(f))
        chrome = [s for s in initial["surfaces"] if s.get("owner_plugin_id") in
                  ("bmux.tab_bar", "bmux.sidebar")]
        native = [s for s in initial["surfaces"] if s.get("owner_plugin_id") is None]
        require(chrome and all(s["selection_available"] and s["semantics_available"] for s in chrome),
                "plugin chrome did not supply selection/semantics metadata")
        require(native and all(not s["selection_available"] and not s["semantics_available"] for s in native),
                "native surfaces falsely claim available metadata")
        require(not initial["unsupported_metadata"] and
                partial_metadata(initial) == ["selection", "semantics"],
                "mixed native/plugin metadata must be partial, not wholly unavailable")
        require(initial["selection"]["scopes"] and initial["selection"]["fragments"] and
                initial["semantics"]["regions"], "real plugin selection/semantics scene missing")
        require(initial["selection_text_available"] and initial["selection_text"],
                "plugin selection source text must accompany same-id ranges")
        require(initial["buffer"]["area"]["width"] == 80 and
                initial["buffer"]["area"]["height"] == 24,
                "headless bootstrap geometry is not 80x24")
        identity = json.loads(wait_for("producer identity", lambda: (
            self.root / "producer.json").read_text() if (self.root / "producer.json").exists() else None))
        self.producer_fd = os.pidfd_open(identity["pid"])
        require(pathlib.Path(f'/proc/{identity["pid"]}/stat').read_text().rsplit(")", 1)[1].split()[19]
                == identity["start"], "owned producer identity changed")
        require(b"\x1b" not in attach.bytes(), "headless attach emitted ANSI")
        fds = [os.readlink(path) for path in pathlib.Path(f"/proc/{attach.child.pid}/fd").iterdir()]
        require(not any(name == "/dev/tty" or name.startswith("/dev/pts/") for name in fds),
                "headless attach opened a terminal/PTY")
        require(any(s.get("owner_plugin_id") == "bmux.tab_bar" for s in initial["surfaces"]),
                "plugin tab bar missing from authoritative surfaces")
        self.passed("independent headless attach: pipes, no terminal/ANSI, plugin chrome")

        profile = self.root / "browser"
        browser = Process([str(self.args.browser), "--headless", "--disable-gpu", "--no-first-run",
                           "--no-default-browser-check", "--remote-debugging-address=127.0.0.1",
                           "--remote-debugging-port=0", "--user-data-dir=" + str(profile), "about:blank"], self.env)
        self.processes.append(browser)
        portfile = profile / "DevToolsActivePort"
        wait_for("Chromium DevTools", lambda: portfile.exists() and browser.child.poll() is None)
        port = int(portfile.read_text().splitlines()[0])
        with urllib.request.build_opener(urllib.request.ProxyHandler({})).open(
                f"http://127.0.0.1:{port}/json", timeout=3) as response:
            pages = json.load(response)
        page = next(p for p in pages if p["type"] == "page" and p["url"] == "about:blank")
        self.client = CDP(page["webSocketDebuggerUrl"])
        self.client.call("Page.enable")
        self.client.call("Network.enable")
        self.client.call("Emulation.setDeviceMetricsOverride", {
            "width": 1000, "height": 720, "deviceScaleFactor": 1, "mobile": False})
        self.client.call("Page.navigate", {"url": url})
        def browser_ready():
            status = self.client.evaluate("document.getElementById('status')?.textContent") or ""
            self.report["browser_bootstrap"] = {"status": status[:160],
                "canvas_present": self.client.evaluate("!!document.getElementById('bmux')"),
                "capability_fragment_removed": self.client.evaluate("location.hash === ''")}
            return status.startswith("Read-only · revision")
        try:
            wait_for("browser bootstrap/WASM", browser_ready)
        except RuntimeError:
            self.report["startup_probe"] = self.client.evaluate("""(async()=>{
 let phase='import',renderer;
 try {const m=await import('/bmux_web_renderer.js');phase='init';await m.default('/bmux_web_renderer_bg.wasm');
 phase='metrics';const v=document.getElementById('viewport'),s=getComputedStyle(v),p=document.createElement('canvas').getContext('2d');
 p.font=`${s.fontSize} ${s.fontFamily}`;const t=p.measureText('M'),h=parseFloat(s.lineHeight),d=devicePixelRatio;
 const a=t.fontBoundingBoxAscent??t.actualBoundingBoxAscent,b=t.fontBoundingBoxDescent??t.actualBoundingBoxDescent;
 if (![t.width,h,d,a,b].every(Number.isFinite)) return {phase:'metrics',error:'non-finite computed font metrics',
 fontSize:s.fontSize,lineHeight:s.lineHeight,cellWidth:t.width,dpr:d};
 phase='constructor';renderer=new m.CanvasRenderer('bmux',t.width*d,h*d);
 phase='set_metrics';renderer.set_metrics(t.width*d,h*d,`${parseFloat(s.fontSize)*d}px ${s.fontFamily}`,Math.max(0,Math.min(h,(h-a-b)/2+a))*d);
 return {phase:'complete',width:t.width,height:h,ascent:a,descent:b};
 } catch(e) {return {phase,error:String(e).replace(/https?:[^\\s]+/g,'URL_REDACTED').slice(0,240)};}
 finally {renderer?.free();}
})()""")
            raise
        frame = self.frame_when("browser measured viewport", lambda f: f["buffer"]["area"] != initial["buffer"]["area"])
        pixels = self.geometry(frame)
        require(pixels["ink"] > 100 and pixels["hash"] == "", "Canvas blank or bootstrap capability retained in URL")
        self.passed("real host + WASM + Canvas pixels + measured viewport")
        self.fixture_command("mark WEB_LIVE_UPDATE")
        frame = self.frame_when("live PTY update", lambda f: "WEB_LIVE_UPDATE" in text(f))
        updated_pixels = self.rendered(frame)
        require(updated_pixels["checksum"] != pixels["checksum"], "PTY update did not change Canvas pixels")
        require(frame["revision"] > initial["revision"], "live revision did not advance")
        self.passed("live PTY output update")

        # An incomplete DEC 2026 transaction must not publish partial output or
        # terminate the output-only attachment. The end marker commits it.
        before_atomic = self.snapshot()
        self.fixture_command("sync-begin")
        time.sleep(0.05)
        require(attach.child.poll() is None, "synchronized update disconnected browser attachment")
        during_atomic = self.snapshot()
        require("WEB_ATOMIC_UPDATE" not in text(during_atomic),
                "incomplete synchronized update was published")
        require(during_atomic["revision"] == before_atomic["revision"],
                "synchronized update advanced committed presentation")
        self.fixture_command("sync-end")
        atomic = self.frame_when("completed synchronized update", lambda f: "WEB_ATOMIC_UPDATE" in text(f))
        require(atomic["revision"] > before_atomic["revision"], "completed update did not commit")
        self.rendered(atomic)
        self.passed("synchronized PTY update retains committed frame and completes without disconnect")

        self.fixture_command("create")
        frame = self.frame_when("Kitty create", lambda f: bool(placements(f)))
        created, _ = self.image_pixels(frame)
        self.fixture_command("move")
        frame = self.frame_when("Kitty move", lambda f: placements(f) and placements(f)[0]["destination"] != created["destination"])
        moved, _ = self.image_pixels(frame)
        # This fixture explicitly deletes and recreates the protocol placement.
        # The producer assigns a fresh retained placement identity to every Place.
        require(moved["key"] != created["key"], "replacement reused deleted placement identity")
        require(all(item["key"] != created["key"] for item in placements(frame)),
                "deleted placement remained in the committed frame")
        self.passed("live Kitty create/placement replacement: decoded resource and Canvas coordinates")

        for width, height, dpr in ((830, 640, 1), (1210, 810, 1), (960, 700, 2)):
            self.client.call("Emulation.setDeviceMetricsOverride", {
                "width": width, "height": height, "deviceScaleFactor": dpr, "mobile": False})
        final_cells = self.client.evaluate("""(() => {
 const v=document.getElementById('viewport'),s=getComputedStyle(v),p=document.createElement('canvas').getContext('2d');
 p.font=`${s.fontSize} ${s.fontFamily}`;
 return [Math.floor(v.clientWidth/p.measureText('M').width),Math.floor(v.clientHeight/parseFloat(s.lineHeight))];
})()""")
        frame = self.frame_when("resize burst final geometry", lambda f:
            [f["buffer"]["area"]["width"], f["buffer"]["area"]["height"]] == final_cells)
        self.geometry(frame)
        self.image_pixels(frame)
        require(self.pixels()["dpr"] == 2, "DPR change did not reach browser")
        self.passed("resize burst + DPR=2 + retained image pixel coordinates")
        # Container/font changes exercise measurement independently of the
        # browser's outer viewport. This is test styling, not an asset edit or
        # a responsive product presentation policy.
        self.client.evaluate("""(() => {
 const v=document.getElementById('viewport');v.style.width='760px';
 v.style.height='500px';v.style.flex='none';v.style.fontSize='16px';v.style.lineHeight='24px';
 return true;
})()""")
        font_cells = self.client.evaluate("""(() => {
 const v=document.getElementById('viewport'),s=getComputedStyle(v),p=document.createElement('canvas').getContext('2d');
 p.font=`${s.fontSize} ${s.fontFamily}`;
 return [Math.floor(v.clientWidth/p.measureText('M').width),Math.floor(v.clientHeight/parseFloat(s.lineHeight))];
})()""")
        frame = self.frame_when("container/font measured geometry", lambda f:
            [f["buffer"]["area"]["width"], f["buffer"]["area"]["height"]] == font_cells)
        self.geometry(frame)
        self.image_pixels(frame)
        self.passed("container/font measurement reaches retained cells and image pixels")
        self.fixture_command("clip")
        frame = self.frame_when("Kitty edge clipping", lambda f: placements(f) and placements(f)[0]["destination"] != moved["destination"])
        clipped, pixels = self.image_pixels(frame)
        require(pixels["red"] < clipped["destination"]["width"] * clipped["destination"]["height"] *
                (pixels["width"]/frame["buffer"]["area"]["width"]) *
                (pixels["height"]/frame["buffer"]["area"]["height"]), "fixture image was not clipped")
        self.fixture_command("remove")
        frame = self.frame_when("Kitty remove", lambda f: not placements(f))
        self.rendered(frame, False)
        self.passed("live Kitty clip/remove: no stale Canvas image resource")

        self.client.call("Network.setBlockedURLs", {"urls": [self.origin + "/snapshot*"]})
        self.client.call("Network.emulateNetworkConditions", {
            "offline": True, "latency": 0, "downloadThroughput": -1, "uploadThroughput": -1})
        # First producer update wakes any already-open long poll; the next poll
        # is blocked. Require observed failure before changing authoritative PTY.
        self.fixture_command("mark WEB_NETWORK_DISCONNECT")
        wait_for("observed browser disconnect", lambda:
            "reconnecting" in (self.pixels() or {}).get("status", ""), 40)
        self.fixture_command("mark WEB_OFFLINE_REPAIR")
        repaired = self.frame_when("offline live update", lambda f: "WEB_OFFLINE_REPAIR" in text(f))
        self.client.call("Network.setBlockedURLs", {"urls": []})
        self.client.call("Network.emulateNetworkConditions", {
            "offline": False, "latency": 0, "downloadThroughput": -1, "uploadThroughput": -1})
        self.rendered(repaired, False)
        self.passed("browser offline reconnect repairs latest complete live output")

        status, data = http(self.origin, "/view", self.token, "POST", b"")
        observer = json.loads(data)
        require(status == 200 and observer["protocol_version"] == 1 and
                not observer["viewport_allowed"] and observer["controller"] is None,
                "second browser viewer obtained existing owner's resize lease")
        self.passed("owner versus extra browser observer: resize authority is exclusive")
        # Use an actual host controller lease after disposing the browser claimant.
        self.client.call("Page.navigate", {"url": "about:blank"})
        def claim():
            status, data = http(self.origin, "/view", self.token, "POST", b"")
            if status != 200:
                return None
            value = json.loads(data)
            return value if value["viewport_allowed"] and value["controller"] else None
        info = wait_for("browser controller release", claim)
        controller, generation = info["controller"], info["generation"]
        require(info["protocol_version"] == 1 and info["lease_ms"] == 30000,
                "host controller protocol/lease differs from Part I contract")
        status, data = http(self.origin, "/view", self.token, "POST", b"", controller)
        require(status == 200 and json.loads(data)["controller"] == controller, "lease renewal failed")
        request = {"generation": generation, "request_id": "1", "cols": 90, "rows": 30}
        require(http(self.origin, "/viewport", self.token, "POST", request, controller)[0] == 204,
                "valid viewport request rejected")
        self.frame_when("owner accepted viewport composed", lambda f:
            f["buffer"]["area"]["width"] == 90 and f["buffer"]["area"]["height"] == 30)
        require(http(self.origin, "/viewport", self.token, "POST", request, controller)[0] == 409,
                "duplicate viewport request accepted")
        stale = dict(request, generation="obsolete-generation", request_id="2")
        require(http(self.origin, "/viewport", self.token, "POST", stale, controller)[0] == 409,
                "stale generation accepted")
        require(http(self.origin, "/snapshot", "invalid-capability")[0] in (401, 403), "invalid bearer accepted")
        require(http(self.origin, "/view", self.token, "POST", b"", request_origin="http://invalid.example")[0] == 403,
                "incorrect Origin accepted")
        require(http(self.origin, "/viewport", self.token, "POST", dict(request, request_id="3"), "invalid-controller")[0] == 400,
                "malformed controller accepted")
        wrong_controller = "0" * 64 if controller != "0" * 64 else "1" * 64
        require(http(self.origin, "/viewport", self.token, "POST", dict(request, request_id="3"), wrong_controller)[0] == 403,
                "invalid controller accepted")
        require(http(self.origin, "/viewport", self.token, "POST", dict(request, request_id=4), controller)[0] == 400,
                "non-string request ID accepted")
        require(http(self.origin, "/view", self.token, "DELETE", b"", controller)[0] == 204, "controller release failed")
        self.passed("real host lease renewal/release, stale generation/request, auth/Origin/controller failures")

        self.old_generation(session, generation)
        self.standalone()
        self.mirror(session)
        self.fixture_command("exit")
        # Stop exactly the foreground daemon in this fresh owned runtime: loss
        # of the native source must invalidate the feed, not retain stale output.
        self.command("server", "stop")
        wait_for("source termination closes headless attach", lambda: attach.child.poll() is not None)
        try:
            status, _ = http(self.origin, "/snapshot", self.token)
            require(status != 200, "terminated source still serves healthy snapshot")
        except (urllib.error.URLError, ConnectionError, TimeoutError):
            pass  # Closed listener is an explicit closed view, not a skipped case.
        require(b"\x1b" not in attach.bytes(), "headless shutdown emitted ANSI")
        self.passed("source termination closes feed and restores output-only lifecycle")

    def old_generation(self, session, old_generation):
        saved = self.origin, self.token
        owner = self.spawn("attach", session, "--web-assets", str(self.args.assets), "--web-only")
        try:
            self.bootstrap(owner)
            self.frame_when("second independent owner", lambda f: bool(f["generation"]))
            def claim():
                status, body = http(self.origin, "/view", self.token, "POST", b"")
                return json.loads(body) if status == 200 else None
            info = wait_for("second owner lease", claim)
            require(info["viewport_allowed"] and info["controller"] and
                    info["generation"] != old_generation, "independent attach reused prior generation/authority")
            request = {"generation": old_generation, "request_id": "1", "cols": 90, "rows": 30}
            require(http(self.origin, "/viewport", self.token, "POST", request, info["controller"])[0] == 409,
                    "real prior-owner generation accepted by replacement owner")
            request["generation"] = info["generation"]
            require(http(self.origin, "/viewport", self.token, "POST", request, info["controller"])[0] == 204,
                    "replacement owner's current generation rejected")
            self.frame_when("independent owner accepted geometry", lambda f:
                f["buffer"]["area"]["width"] == 90 and f["buffer"]["area"]["height"] == 30)
            self.passed("independent owner generation: real prior-owner request rejected")
        finally:
            owner.stop()
            self.origin, self.token = saved

    def standalone(self):
        """Exercise bootstrap generation repair with genuine example output.

        The fixture serves only existing assets/example JSON. Live host cases
        above remain mandatory; this fixture is not evidence of native reconnect.
        """
        source = json.loads(self.args.standalone_snapshot.read_text())
        require(unsupported_metadata(source) == [] and source["selection_text_available"] and
                source["selection_text"] and source["selection"]["fragments"],
                "standalone snapshot must supply complete metadata and selection source text")
        require(source["generation"] == "standalone-example" and placements(source),
                "standalone snapshot must come from the existing snapshot example")
        source["revision"] = 10
        state = {"frame": source, "transport": 1}
        capability = os.urandom(32).hex()
        assets = self.args.assets
        helper_content = self.args.canvas_conformance.read_text()
        class Handler(http_server.BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def serve(self):
                name = "index.html" if self.path == "/" else self.path[1:]
                if name == "canvas-conformance.js" and self.command == "GET":
                    body, code, mime = helper_content.encode(), 200, "text/javascript"
                elif name in ASSETS and self.command == "GET":
                    body, code, mime = (assets / name).read_bytes(), 200, ASSETS[name]
                elif self.headers.get("Authorization") != "Bearer " + capability:
                    body, code, mime = b"", 403, "text/plain"
                elif self.path == "/view" and self.command == "POST":
                    body = json.dumps({"generation": state["frame"]["generation"],
                        "protocol_version": 1, "viewport_allowed": False, "controller": None, "lease_ms": 0}).encode()
                    code, mime = 200, "application/json"
                elif self.path.startswith("/snapshot") and self.command == "GET":
                    after = urllib.parse.parse_qs(urllib.parse.urlsplit(self.path).query).get("after")
                    if after == [str(state["transport"])]:
                        body, code, mime = b"", 204, "application/json"
                    else:
                        body, code, mime = json.dumps(state["frame"]).encode(), 200, "application/json"
                else:
                    body, code, mime = b"", 404, "text/plain"
                self.send_response(code)
                self.send_header("Content-Type", mime)
                self.send_header("Content-Length", str(len(body)))
                if self.path.startswith("/snapshot"):
                    self.send_header("X-Snapshot-Revision", str(state["transport"]))
                self.end_headers()
                try:
                    self.wfile.write(body)
                except (BrokenPipeError, ConnectionResetError):
                    pass
            do_GET = serve
            do_POST = serve
        self.fixture_server = http_server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.fixture_server.daemon_threads = True
        threading.Thread(target=self.fixture_server.serve_forever, daemon=True).start()
        origin = f"http://127.0.0.1:{self.fixture_server.server_port}"
        self.client.call("Page.navigate", {"url": origin + "/#token=" + capability})
        wait_for("standalone example bootstrap committed", lambda:
            (self.pixels() or {}).get("status") == "Read-only · revision 10" and
            self.client.evaluate("location.origin") == origin)
        # Keep standalone diagnostics separate from the restored native tab.
        self.report["standalone_stage"] = "initial commit"
        pixels = self.rendered(source)
        require(pixels["ink"] > 100, "standalone TUI consumer rendered blank")
        # Begin a replacement execution with a lower presentation revision and
        # no image. Same bootstrap must reset revision/resource authority.
        replacement = copy.deepcopy(source)
        replacement.update(generation="replacement-example", revision=1)
        replacement["image_scene"]["active"] = {}
        state.update(frame=replacement, transport=2)
        # Lower revision must actually commit; a >= check would accidentally
        # accept the old generation's still-visible revision 10.
        wait_for("bootstrap replacement-generation commit", lambda:
                 (self.pixels() or {}).get("status") == "Read-only · revision 1")
        self.rendered(replacement)
        require(self.client.evaluate("""(async()=>{const {CanvasRenderer}=await import('/bmux_web_renderer.js');
 return JSON.parse(CanvasRenderer.inspect_json(""" + json.dumps(json.dumps(replacement)) + ")).generation;})()")
                == "replacement-example", "WASM decoded replacement generation incorrectly")
        self.passed("standalone TUI example + bootstrap generation/lower-revision repair")
        # Separate document prevents the polling bootstrap from interfering with
        # ImageBitmap resource accounting in the real-WASM conformance helper.
        self.client.call("Page.navigate", {"url": origin + "/bmux_web_renderer.js"})
        wait_for("conformance document", lambda: self.client.evaluate(
            "location.pathname === '/bmux_web_renderer.js' && document.readyState === 'complete'"))
        # The fixture owns this extra URL; production asset allowlists are not
        # modified. Import the existing test helper as an ES module verbatim.
        second = copy.deepcopy(source)
        second["revision"] = 11
        expression = ("(async()=>{const m=await import('/bmux_web_renderer.js');await m.default();"
                      "const c=document.createElement('canvas');c.id='conformance';document.body.append(c);"
                      "const h=await import('/canvas-conformance.js');"
                      "await h.canvasConformance(m.CanvasRenderer,'conformance',"
                      + json.dumps(source) + "," + json.dumps(second) + ","
                      + json.dumps(replacement) + ");return true;})()")
        require(self.client.evaluate(expression) is True, "actual WASM Canvas conformance failed")
        self.passed("actual WASM failed-copy rollback/metrics/reset/repair/Drop ImageBitmap conformance")
        self.client.call("Page.navigate", {"url": "about:blank"})
        self.fixture_server.shutdown()
        self.fixture_server.server_close()
        self.fixture_server = None

    def mirror(self, session):
        import fcntl
        import pty
        import struct
        import termios
        master, slave = pty.openpty()
        child = None
        try:
            fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 32, 100, 0, 0))
            before = termios.tcgetattr(slave)
            child = subprocess.Popen(self.argv("attach", session, "--web-assets", str(self.args.assets)),
                env=self.env, stdin=slave, stdout=slave, stderr=slave, start_new_session=True)
            output = bytearray()
            def bootstrap():
                require(child.poll() is None, "terminal mirror exited prematurely")
                if select.select([master], [], [], 0.05)[0]:
                    output.extend(os.read(master, 65536))
                    require(len(output) < 1024 * 1024, "mirror output exceeded bound")
                return re.search(rb"http://127\.0\.0\.1:\d+/[^\s\x1b]*token=[^\s\x1b]+", output)
            match = wait_for("terminal mirror bootstrap", bootstrap)
            parts = urllib.parse.urlsplit(match[0].decode())
            origin = "http://" + parts.netloc
            token = urllib.parse.parse_qs(parts.fragment)["token"][0]
            def view():
                status, data = http(origin, "/view", token, "POST", b"")
                return json.loads(data) if status == 200 else None
            info = wait_for("terminal mirror observer mode", view)
            require(not info["viewport_allowed"] and info["controller"] is None,
                    "mirror improperly grants browser resize authority")
            require(http(origin, "/viewport", token, "POST", {"generation": info["generation"],
                    "request_id": "1", "cols": 80, "rows": 24})[0] == 403,
                    "mirror accepted browser viewport mutation")
            wait_for("terminal mirror ANSI rendering", lambda: bootstrap() and b"\x1b[" in output)
            # Existing prefix Ctrl-A + d detach command, not browser input.
            os.write(master, b"\x01d")
            def detached():
                if select.select([master], [], [], 0.05)[0]:
                    output.extend(os.read(master, 65536))
                return child.poll() is not None
            wait_for("clean terminal mirror detach", detached)
            require(child.returncode == 0 and termios.tcgetattr(slave) == before,
                    "terminal mirror did not exit cleanly/restore termios")
            self.passed("terminal mirror ANSI, observer-mode failure, clean detach/termios restoration")
        finally:
            if child and child.poll() is None:
                os.killpg(child.pid, signal.SIGTERM)
                try:
                    child.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    os.killpg(child.pid, signal.SIGKILL)
                    child.wait(timeout=5)
            os.close(master)
            os.close(slave)

    def close(self):
        failures = []
        if self.client:
            self.client.close()
        if self.fixture_server:
            self.fixture_server.shutdown()
            self.fixture_server.server_close()
        # Graceful daemon shutdown uses only the fresh, pinned owned config.
        try:
            self.command("server", "stop", check=False)
        except (OSError, subprocess.TimeoutExpired, RuntimeError):
            failures.append("owned daemon graceful stop failed")
        if self.producer_fd is not None:
            try:
                signal.pidfd_send_signal(self.producer_fd, signal.SIGKILL)
                if not select.select([self.producer_fd], [], [], 5)[0]:
                    failures.append("owned producer did not terminate")
            except ProcessLookupError:
                pass
            finally:
                os.close(self.producer_fd)
        for process in reversed(self.processes):
            try:
                process.stop()
            except (OSError, subprocess.TimeoutExpired):
                failures.append("owned process cleanup failed")
        self.report["cleanup"] = "FAIL" if failures else "PASS"
        if failures:
            self.report["result"] = "FAIL"
        if self.args.evidence:
            self.args.evidence.write_text(json.dumps(self.report, indent=2) + "\n")
        # No raw daemon/attach/browser logs escape the private owned sandbox.
        shutil.rmtree(self.root)
        require(not failures, "owned runtime cleanup failed")


def main():
    if len(sys.argv) == 3 and sys.argv[1] == "--producer":
        producer(sys.argv[2])
        return
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=pathlib.Path, required=True, help="explicit prebuilt bmux executable")
    parser.add_argument("--assets", type=pathlib.Path, required=True, help="explicit built WASM asset directory")
    parser.add_argument("--browser", type=pathlib.Path, required=True, help="explicit Chromium executable")
    parser.add_argument("--standalone-snapshot", type=pathlib.Path, required=True,
                        help="JSON stdout from bmux_web_renderer example snapshot")
    parser.add_argument("--canvas-conformance", type=pathlib.Path,
                        default=pathlib.Path(__file__).resolve().parents[1] /
                        "packages/web-renderer/tests/canvas-conformance.js",
                        help="existing actual-WASM Canvas conformance helper")
    parser.add_argument("--evidence", type=pathlib.Path, help="new sanitized JSON report path (never raw logs)")
    parser.add_argument("--timeout", type=int, default=180, help="total wall seconds, including assertions")
    args = parser.parse_args()
    for name in ("binary", "assets", "browser", "standalone_snapshot", "canvas_conformance"):
        setattr(args, name, getattr(args, name).resolve(strict=True))
    for name in ("binary", "browser"):
        require(os.access(getattr(args, name), os.X_OK), name + " is not executable")
    for name in ASSETS:
        require((args.assets / name).is_file(), "missing built asset: " + name)
    require(sys.platform == "linux" and hasattr(os, "pidfd_open") and hasattr(signal, "pidfd_send_signal"),
            "Linux pidfd process ownership is required")
    require(args.timeout >= 30, "wall timeout must be at least 30 seconds")
    if args.evidence:
        args.evidence = args.evidence.absolute()
        # Refuse to overwrite another lane's or previous run's evidence.
        with args.evidence.open("x"):
            pass
    harness = Harness(args)
    def interrupted(_signal, _frame):
        raise RuntimeError("acceptance wall limit or cancellation")
    signal.signal(signal.SIGALRM, interrupted)
    signal.signal(signal.SIGTERM, interrupted)
    signal.signal(signal.SIGINT, interrupted)
    signal.alarm(args.timeout)
    try:
        harness.run()
    except BaseException:
        harness.report["result"] = "FAIL"
        raise
    else:
        harness.report["result"] = "PASS"
    finally:
        signal.alarm(0)
        harness.close()
    print("PASS: all Part I browser acceptance cases; owned runtime cleaned", flush=True)


if __name__ == "__main__":
    try:
        main()
    except BaseException as error:
        if isinstance(error, SystemExit):
            raise
        # Exception messages/payloads can include URLs and controller secrets.
        # Emit only safe harness-authored messages, never tracebacks/log dumps.
        message = str(error) if type(error) in (RuntimeError, CDP_MODULE.CDPError) else type(error).__name__
        print("FAIL: " + message, file=sys.stderr)
        sys.exit(1)
