#!/usr/bin/env python3
"""Isolated source-to-Canvas-commit evidence; no build/install/user runtime.

Coordinator only, after native example/assets are built:
python3 scripts/live-components-performance.py --binary /absolute/live-publication-performance \
    --assets /absolute/target/web-assets --browser /absolute/chromium --samples 32

The selected binary is the focused web-host example, NOT bmux CLI. It starts only
an ephemeral loopback feed in an isolated environment. Reuses existing CDP and
bounded browser-process helpers. No tokens, URLs, CDP payloads or child logs print.

Source timestamps are Linux CLOCK_MONOTONIC; Python monotonic_ns uses that clock.
Browser performance.now is calibrated to it using bounded CDP round trips (minimum
RTT midpoint, with explicit half-RTT uncertainty). Canvas-commit sequence is read
from bootstrap's status update AFTER await renderer.present_binary, observed by a
MutationObserver. This measures source publication -> Canvas API commit observation,
NOT screen scanout/physical paint. The host-observed latency is a separate upper
bound including CDP observation overhead; no wall-clock subtraction is used.
All timing is evidence, not a pass/fail performance threshold. Deadlines only bound
failed protocol waits. No benchmark claim is implied until coordinator execution.
"""
import argparse
import importlib.util
import json
import os
from pathlib import Path
import queue
import signal
import subprocess
import sys
import tempfile
import threading
import time
import urllib.request

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parent.parent


def load(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


WEB = load("live_performance_web_helpers", ROOT / "scripts/web-browser-acceptance.py")
require, wait_for = WEB.require, WEB.wait_for


class Actor:
    """Own the explicitly selected harness child; bounded, capability-safe protocol."""
    def __init__(self, argv, env):
        self.child = subprocess.Popen(argv, env=env, stdin=subprocess.PIPE,
                                      stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                      start_new_session=True)
        self.events = queue.Queue(maxsize=16)
        self.failed = threading.Event()
        self.threads = [threading.Thread(target=self.drain, daemon=True),
                        threading.Thread(target=self.discard_errors, daemon=True)]
        for thread in self.threads:
            thread.start()

    def drain(self):
        while True:
            line = self.child.stdout.readline(8193)
            if not line:
                return
            if len(line) > 8192 or not line.endswith(b"\n"):
                self.failed.set()
                return
            try:
                event = json.loads(line)
                if not isinstance(event, dict):
                    raise ValueError()
                self.events.put(event, timeout=1)
            except (ValueError, queue.Full):
                self.failed.set()
                return

    def discard_errors(self):
        # Drain without retaining private logs or allowing child pipe backpressure.
        while self.child.stderr.read(65536):
            pass

    def event(self):
        require(not self.failed.is_set(), "harness protocol exceeded bounds")
        try:
            return self.events.get(timeout=15)
        except queue.Empty:
            raise RuntimeError("harness protocol wait expired") from None

    def publish(self, command):
        require(command in ("steady", "change", "image-steady", "image-change", "reset"),
                "invalid driver command")
        self.child.stdin.write((command + "\n").encode())
        self.child.stdin.flush()
        event = self.event()
        require(isinstance(event.get("published_source_revision"), int) and
                isinstance(event.get("source_monotonic_ns"), int), "invalid publication receipt")
        return event

    def close(self):
        if self.child.poll() is None:
            try:
                self.child.stdin.write(b"quit\n")
                self.child.stdin.flush()
                self.child.stdin.close()
                self.child.wait(timeout=5)
            except (OSError, subprocess.TimeoutExpired):
                os.killpg(self.child.pid, signal.SIGTERM)
                try:
                    self.child.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    os.killpg(self.child.pid, signal.SIGKILL)
                    self.child.wait(timeout=5)
        for thread in self.threads:
            thread.join(timeout=1)
        for stream in (self.child.stdin, self.child.stdout, self.child.stderr):
            if not stream.closed:
                stream.close()


INSTALL = r"""(()=>{
 const status=document.getElementById('status');
 if(!status)throw new Error('status unavailable');
 const records=[];
 const observe=()=>{
   const match=/Read-only · revision ([0-9]+)/.exec(status.textContent);
   if(match){
     const seq=match[1];
     if(records.length && records[records.length-1].seq===seq)return;
     records.push({seq,commit_observed_ms:performance.now()});
     if(records.length>32)records.shift();
   }
 };
 const observer=new MutationObserver(observe);
 observer.observe(status,{childList:true,subtree:true,characterData:true});
 window.__livePerformance={records,observer};
 observe();
 return true;
})()"""


def calibration(client):
    probes = []
    for _ in range(7):
        before = time.monotonic_ns()
        browser_ms = client.evaluate("performance.now()")
        after = time.monotonic_ns()
        probes.append((after - before, (before + after) // 2 - round(browser_ms * 1_000_000)))
    rtt, offset = min(probes)
    return {"offset_ns": offset, "uncertainty_ns": (rtt + 1) // 2,
            "minimum_roundtrip_ns": rtt, "clock": "CLOCK_MONOTONIC <-> performance.now"}


def committed(client, seq):
    # Sequence travels through actual prepared publication -> feed -> encoding ->
    # bootstrap -> WASM Canvas -> status. No production instrumentation is added.
    return wait_for("Canvas committed source sequence", lambda: client.evaluate(
        "window.__livePerformance.records.find(r=>r.seq===" + json.dumps(str(seq)) + ") || null"))


def measure(actor, client, command, clock):
    receipt = actor.publish(command)
    seq = receipt["published_source_revision"]
    record = committed(client, seq)
    observed_ns = time.monotonic_ns()
    commit_ns = round(record["commit_observed_ms"] * 1_000_000) + clock["offset_ns"]
    source_ns = receipt["source_monotonic_ns"]
    latency = commit_ns - source_ns
    require(latency >= -clock["uncertainty_ns"], "clock mapping exceeded stated uncertainty")
    return {"source_revision": seq, "source_monotonic_ns": source_ns,
            "canvas_commit_observed_monotonic_ns": commit_ns,
            "source_to_canvas_commit_observed_ns": latency,
            "source_to_host_observed_upper_bound_ns": observed_ns - source_ns}


def summary(values, field):
    ordered = sorted(value[field] for value in values)
    def rank(percent):
        return ordered[(len(ordered) * percent + 99) // 100 - 1]
    return {"count": len(ordered), "p50_ns": rank(50), "p95_ns": rank(95)}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--assets", required=True, type=Path)
    parser.add_argument("--browser", required=True, type=Path)
    parser.add_argument("--samples", type=int, default=32)
    args = parser.parse_args()
    require(sys.platform.startswith("linux") and "CLOCK_MONOTONIC" in time.get_clock_info("monotonic").implementation,
            "shared CLOCK_MONOTONIC evidence requires Linux")
    require(2 <= args.samples <= 128, "samples must be 2..128")
    for path in (args.binary, args.browser):
        require(path.is_absolute() and path.is_file() and os.access(path, os.X_OK),
                "binary/browser must be explicit absolute executable files")
    require(args.assets.is_absolute() and args.assets.is_dir(), "explicit absolute asset directory required")
    for name in WEB.ASSETS:
        require((args.assets / name).is_file(), "selected asset missing")
    identity = {"binary_sha256": WEB.digest(args.binary), "browser_sha256": WEB.digest(args.browser),
                "assets_sha256": {name: WEB.digest(args.assets / name) for name in WEB.ASSETS}}
    actor = browser = client = None
    with tempfile.TemporaryDirectory(prefix="bmux-live-performance-") as directory:
        root = Path(directory)
        env = os.environ.copy()
        # No inherited user config, runtime endpoints, auth/proxy settings or plugins.
        for key in list(env):
            if key.startswith(("BMUX", "BCODE", "XDG_")) or key.lower().endswith("_proxy"):
                del env[key]
        env.update({"HOME": str(root), "XDG_CONFIG_HOME": str(root / "config"),
                    "XDG_DATA_HOME": str(root / "data"), "XDG_STATE_HOME": str(root / "state"),
                    "XDG_CACHE_HOME": str(root / "cache"), "XDG_RUNTIME_DIR": str(root / "runtime"),
                    "TMPDIR": str(root / "tmp"), "NO_PROXY": "127.0.0.1,localhost"})
        for name in ("config", "data", "state", "cache", "runtime", "tmp"):
            (root / name).mkdir(mode=0o700)
        try:
            actor = Actor([str(args.binary), str(args.assets), "--serve"], env)
            ready = actor.event()
            url = ready.get("ready")
            require(isinstance(url, str) and url.startswith("http://127.0.0.1:"),
                    "harness must own ephemeral loopback host")
            profile = root / "browser-profile"
            browser = WEB.Process([str(args.browser), "--headless", "--disable-gpu", "--no-first-run",
                "--no-default-browser-check", "--remote-debugging-address=127.0.0.1",
                "--remote-debugging-port=0", "--user-data-dir=" + str(profile), "about:blank"], env)
            portfile = profile / "DevToolsActivePort"
            wait_for("isolated Chromium", lambda: portfile.exists() and browser.child.poll() is None)
            port = int(portfile.read_text().splitlines()[0])
            opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
            with opener.open(f"http://127.0.0.1:{port}/json", timeout=3) as response:
                pages = json.load(response)
            page = next(p for p in pages if p["type"] == "page" and p["url"] == "about:blank")
            client = WEB.CDP(page["webSocketDebuggerUrl"])
            client.call("Page.enable")
            client.call("Emulation.setDeviceMetricsOverride", {"width": 1000, "height": 720,
                        "deviceScaleFactor": 1, "mobile": False})
            client.call("Page.navigate", {"url": url})
            wait_for("real WASM Canvas initial commit", lambda: client.evaluate(
                "document.getElementById('status')?.textContent?.includes('Read-only · revision 1')"))
            require(client.evaluate(INSTALL), "commit sequence observer unavailable")
            results = []
            for image in (False, True):
                steady, change = (("image-steady", "image-change") if image else ("steady", "change"))
                admission_clock = calibration(client)
                admission = measure(actor, client, steady, admission_clock)
                for changing in (False, True):
                    # Separate admission/warmup from reuse; each changed update alternates
                    # payload/cell state so it cannot accidentally measure unchanged reuse.
                    clock = calibration(client)
                    samples = [measure(actor, client, change if changing and i % 2 == 0 else steady, clock)
                               for i in range(args.samples)]
                    ending_clock = calibration(client)
                    results.append({"image": image, "change": changing, "consumer": "read_each_update",
                        "clock_mapping": clock, "ending_clock_mapping": ending_clock,
                        "clock_offset_drift_ns": ending_clock["offset_ns"] - clock["offset_ns"],
                        "first_workload_admission": admission if not changing else None,
                        "source_to_canvas_commit_observed": summary(samples, "source_to_canvas_commit_observed_ns"),
                        "source_to_host_observed_upper_bound": summary(samples, "source_to_host_observed_upper_bound_ns"),
                        "samples": samples})
            # Freeze only this isolated browser. Bursts use the real latest-only feed;
            # native harness proves exact predecessor versus full bytes deterministically.
            slow_clock = calibration(client)
            client.call("Page.setWebLifecycleState", {"state": "frozen"})
            burst = [actor.publish("image-change" if i % 2 == 0 else "image-steady") for i in range(8)]
            client.call("Page.setWebLifecycleState", {"state": "active"})
            latest = committed(client, burst[-1]["published_source_revision"])
            slow = {"consumer": "browser_frozen_during_8_publications", "publication_count": 8,
                    "latest_committed_source_revision": latest["seq"],
                    "source_to_canvas_commit_observed_ns": round(latest["commit_observed_ms"] * 1_000_000) + slow_clock["offset_ns"] - burst[-1]["source_monotonic_ns"],
                    "source_to_host_observed_upper_bound_ns": time.monotonic_ns() - burst[-1]["source_monotonic_ns"],
                    "clock_mapping": slow_clock,
                    "note": "latest visual repair; no assertion that browser read every intermediate update"}
            reset = measure(actor, client, "reset", calibration(client))
            print(json.dumps({"schema": "bmux.live-components-browser-performance.v1", "identity": identity,
                "metric_definition": "source CLOCK_MONOTONIC -> status MutationObserver after present_binary Canvas API commit; not physical paint",
                "percentiles": "nearest rank", "results": results, "slow_consumer": slow,
                "generation_reset_full_repair": reset}, separators=(",", ":")))
        finally:
            if client:
                client.close()
            if browser:
                browser.stop()
            if actor:
                actor.close()


if __name__ == "__main__":
    try:
        main()
    except Exception:
        # Never echo exception bodies: selected processes/URLs can contain secrets.
        print("live-components performance measurement failed (private details withheld)", file=sys.stderr)
        sys.exit(1)
