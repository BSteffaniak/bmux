#!/usr/bin/env python3
"""Real-browser regression for retained document layout during transport loss."""
import argparse
import hashlib
import http.server
import importlib.util
import json
import pathlib
import subprocess
import sys
import tempfile
import threading
import time
import urllib.request

sys.dont_write_bytecode = True
ROOT = pathlib.Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location("cdp", ROOT / "scripts/web-browser-cdp.py")
CDP = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(CDP)


def wait_for(predicate):
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline:
        if predicate():
            return
        time.sleep(0.05)
    raise AssertionError("browser condition timed out")


class Handler(http.server.BaseHTTPRequestHandler):
    online = True
    resource = b'HTTP LAN verified resource'
    corrupt = False
    resource_id = hashlib.sha256(resource).hexdigest()

    def log_message(self, *_args):
        pass

    def do_GET(self):
        path = self.path.split("?")[0]
        mime = "text/javascript"
        if path == "/document":
            if not self.online:
                self.send_error(503)
                return
            # Long-poll unchanged publications rather than spin the client.
            if "after=" in self.path:
                time.sleep(0.1)
                self.send_response(204)
                self.end_headers()
                return
            data = json.dumps(dict(version=1, publication_revision="1", generation="test",
                                   capability="test", revision="1", content=[1], resources=[dict(
                                       id=self.resource_id, mime_type='application/octet-stream',
                                       length=len(self.resource))])).encode()
            mime = "application/vnd.bmux.document-v1+json"
        elif path == '/resource/' + self.resource_id:
            data = b'X' + self.resource[1:] if self.corrupt else self.resource
            mime = 'application/octet-stream'
        elif path == "/test-adapter.js":
            data = b"""export function createDocumentRenderer() {
              window.draws = [];
              return {capability: 'test', prepare: async (d, v, host) => {
                await host.requestResource(d.resources[0].id);
                return {commit() { window.draws.push(v); }, discard() {}};
              }, reset() {}, free() {}};
            }"""
        else:
            file = ROOT / "packages/web-renderer/browser" / ("index.html" if path == "/" else path[1:])
            if path not in ("/", "/style.css", "/bootstrap.js"):
                self.send_error(404)
                return
            data = file.read_bytes()
            mime = "text/html" if path == "/" else "text/css" if path.endswith(".css") else mime
        self.send_response(200)
        self.send_header("Content-Type", mime)
        if path == "/document":
            self.send_header("X-Snapshot-Revision", "1")
        self.end_headers()
        self.wfile.write(data)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--browser", required=True)
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="bmux-reconnect-test-") as profile:
        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        browser = subprocess.Popen([args.browser, "--headless", "--disable-gpu", "--no-first-run",
                                    '--host-resolver-rules=MAP bmux-lan.test 127.0.0.1', '--no-proxy-server',
                                    "--remote-debugging-address=127.0.0.1", "--remote-debugging-port=0",
                                    "--user-data-dir=" + profile, "about:blank"],
                                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        client = None
        try:
            portfile = pathlib.Path(profile) / "DevToolsActivePort"
            wait_for(portfile.exists)
            port = int(portfile.read_text().splitlines()[0])
            with urllib.request.urlopen(f"http://127.0.0.1:{port}/json", timeout=3) as response:
                page = next(p for p in json.load(response) if p["type"] == "page")
            client = CDP.CDP(page["webSocketDebuggerUrl"])
            client.call("Page.navigate", {"url": f"http://bmux-lan.test:{server.server_port}/?delivery=document&adapter=/test-adapter.js"})
            evaluate = client.evaluate
            wait_for(lambda: evaluate("window.draws?.length > 0"))
            assert evaluate('isSecureContext') is False
            assert evaluate('typeof crypto.subtle') == 'undefined'
            time.sleep(0.2)
            baseline = evaluate("({count:draws.length,height:document.getElementById('viewport').clientHeight})")
            Handler.online = False
            wait_for(lambda: evaluate("window.__bmuxDiagnostics().events.some(e => e.code === 'document_repair' && e.failures >= 3)"))
            assert evaluate("draws.length") == baseline["count"], "retries redrew unchanged content"
            assert evaluate("document.getElementById('viewport').clientHeight") == baseline["height"], "status resized viewport"
            # A genuine local resize must still reflow without claiming recovery.
            client.call("Emulation.setDeviceMetricsOverride", {"width": 350, "height": 600, "deviceScaleFactor": 1, "mobile": False})
            wait_for(lambda: evaluate(f"draws.length > {baseline['count']}"))
            time.sleep(0.2)
            assert "Document unavailable" in evaluate("document.getElementById('status').textContent")
            assert "recovered" not in evaluate("window.__bmuxDiagnostics().events.map(e => e.code)")
            Handler.online = True
            wait_for(lambda: evaluate("window.__bmuxDiagnostics().events.some(e => e.code === 'recovered')"))
            assert "Read-only" in evaluate("document.getElementById('status').textContent")
            # A fresh page has no cached bytes: corruption must prevent commit.
            Handler.corrupt = True
            client.call('Page.reload')
            wait_for(lambda: evaluate("document.getElementById('status').textContent.includes('resource digest mismatch')"))
            assert evaluate('draws.length') == 0, 'corrupt resource was committed'
            print("browser HTTP-LAN resource verification, corruption, reconnect: PASS")
        finally:
            if client:
                client.close()
            browser.terminate()
            try:
                browser.wait(timeout=8)
            except subprocess.TimeoutExpired:
                browser.kill()
                browser.wait()
            server.shutdown()
            server.server_close()


if __name__ == "__main__":
    main()
