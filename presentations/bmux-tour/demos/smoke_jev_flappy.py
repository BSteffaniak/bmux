#!/usr/bin/env python3
"""Owned managed-presenter rehearsal with fake loopback Jev, never real credentials.

Requires an explicit freshly built BMUX binary and matching plugin directory.
Retains private bounded PTY/runtime evidence. Never targets an existing runtime.
"""
import argparse
import importlib.util
import json
import os
from pathlib import Path
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

ROOT = Path(__file__).resolve().parents[3]
SPEC = importlib.util.spec_from_file_location("presenter_helpers", ROOT / "scripts/presenter-web-acceptance.py")
HELPERS = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(HELPERS)
APP_SPEC = importlib.util.spec_from_file_location("flappy_app", Path(__file__).with_name("jev_flappy.py"))
APP = importlib.util.module_from_spec(APP_SPEC)
# dataclasses resolves its defining module through sys.modules.
import sys
sys.modules[APP_SPEC.name] = APP
APP_SPEC.loader.exec_module(APP)


def wait(label, predicate, timeout=90):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        result = predicate()
        if result:
            return result
        time.sleep(0.1)
    raise AssertionError("Timed out: " + label)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=lambda p: Path(p).resolve(strict=True), required=True)
    parser.add_argument("--plugins", type=lambda p: Path(p).resolve(strict=True), required=True)
    args = parser.parse_args()
    root = Path(tempfile.mkdtemp(prefix="jf-", dir="/tmp"))
    root.chmod(0o700)
    print("Private smoke evidence:", root, flush=True)
    count = [0]
    delay = [0]

    class Handler(BaseHTTPRequestHandler):
        def do_POST(self):
            payload = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            assert self.path == "/api/v1/decide"
            assert self.headers["Authorization"] == "Bearer smoke-only-key"
            assert payload["questions"]["action"]["type"] == "choice"
            count[0] += 1
            time.sleep(delay[0])
            # Deliberately bad pilot proves game-over and automatic retry.
            body = json.dumps({"model": payload["model"], "answers": {
                "action": {"type": "choice", "choice": "WAIT", "confidence": 1.0,
                           "probabilities": {"FLAP": 0.0, "WAIT": 1.0}}}}).encode()
            try:
                self.send_response(200)
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)
            except (BrokenPipeError, ConnectionResetError):
                pass

        def log_message(self, *_):
            pass

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    process = None
    env = {k: v for k, v in os.environ.items() if k in ("PATH", "LANG", "LC_ALL")}
    for name in ("home", "config", "runtime", "data", "state", "logs", "tmp"):
        (root / name).mkdir(mode=0o700)
    env.update(HOME=str(root / "home"), TMPDIR=str(root / "tmp"), TERM="xterm-256color",
               SHELL="/bin/sh", BMUX_NO_BASE_CONFIG="1", BMUX_RUNTIME_NAME="default", BMUX_TARGET="local")
    for name in ("config", "runtime", "data", "state", "log"):
        env["BMUX_" + name.upper() + "_DIR"] = str(root / ("logs" if name == "log" else name))
    config = root / "config/bmux.toml"
    config.write_text('[plugins]\nenabled = ["bmux.presenter", "bmux.playbook", "bmux.sessions", "bmux.contexts", "bmux.clients", "bmux.tabs"]\nsearch_paths = '
                      + json.dumps([str(args.plugins)]) + '\n')
    env["BMUX_CONFIG"] = str(config)
    for key, value in {"JEV_API_KEY": "smoke-only-key", "JEV_BASE_URL": f"http://127.0.0.1:{server.server_port}"}.items():
        env[key] = value
    (root / "jev_flappy.py").write_bytes(Path(__file__).with_name("jev_flappy.py").read_bytes())
    (root / "jev-flappy.dsl").write_bytes(Path(__file__).with_name("jev-flappy.dsl").read_bytes())
    deck = root / "deck.md"
    deck.write_text('# Jev rehearsal\n\n<!-- bmux:block-id demo-jev -->\n'
                    '<!-- bmux:component playbook-demo {"playbook":"jev-flappy.dsl","on_leave":"stop"} -->\n'
                    '\n---\n# After demo\n')
    command = [str(args.binary), "--runtime", "default", "--target", "local", "--config", str(config)]
    try:
        process = HELPERS.Mirror([*command, "present", "play", str(deck), "--allow-playbooks", "--no-reload"], env)
        wait("presenter initial frame", lambda: b"Jev rehearsal" in process.bytes())
        process.send(b"\t")
        time.sleep(0.2)
        process.send(b"o")
        wait("expanded demo controls", lambda: b"Preview" in process.bytes())
        time.sleep(1)
        process.send(b"r")
        def runner():
            for file in root.rglob("runner.json"):
                value = json.loads(file.read_text())
                if value.get("phase") == "script-complete":
                    return file
            return None
        evidence = wait("real game startup and script completion", runner)
        def game_socket():
            for locator in root.rglob(".jev-flappy-control.json"):
                path = json.loads(locator.read_text())["socket"]
                if Path(path).exists():
                    return path
            return None
        control_path = wait("generated control socket", game_socket)
        wait("provider requests", lambda: count[0] >= 3)
        status = APP.send_control(control_path, "pause")
        assert status["paused"] and not status["in_flight"]
        calls = count[0]
        time.sleep(1)
        assert count[0] == calls
        APP.send_control(control_path, "restart")
        APP.send_control(control_path, "resume")
        wait("automatic next attempt", lambda: APP.send_control(control_path, "status")["attempt"] >= 3)
        assert json.loads(evidence.read_text())["phase"] == "script-complete"
        # The startup deadline is 45s. Retention must outlive that deadline.
        APP.send_control(control_path, "pause")
        time.sleep(46)
        assert json.loads(evidence.read_text())["phase"] == "script-complete"
        assert not json.loads(evidence.read_text())["cleanup_acknowledged"]
        APP.send_control(control_path, "resume")
        # Stop via normal presenter navigation while a request is outstanding.
        delay[0] = 3
        wait("in-flight decision", lambda: APP.send_control(control_path, "status")["in_flight"])
        process.send(b"\x1b")
        time.sleep(0.3)
        process.send(b"\x1b[C")
        wait("on-leave cleanup", lambda: json.loads(evidence.read_text()).get("cleanup_acknowledged"), 30)
        calls = count[0]
        time.sleep(4)
        assert count[0] == calls
        # Re-entering is inert until explicit Restart; test closing the entire
        # presenter with a fresh live request as a separate cleanup boundary.
        process.send(b"\x1b[D")
        time.sleep(1)
        process.send(b"\t")
        time.sleep(0.2)
        process.send(b"o")
        time.sleep(1)
        process.send(b"R")
        previous = evidence
        def restarted_runner():
            for file in root.rglob("runner.json"):
                if file != previous and json.loads(file.read_text()).get("phase") == "script-complete":
                    return file
            return None
        evidence = wait("explicit restart after re-entry", restarted_runner)
        control_path = wait("restarted generated control socket", game_socket)
        wait("request after re-entry", lambda: APP.send_control(control_path, "status")["in_flight"])
        process.send(b"q")
        wait("presenter exit", lambda: process.child.poll() is not None, 40)
        assert process.child.returncode == 0
        assert json.loads(evidence.read_text())["cleanup_acknowledged"]
        print("PASS: managed startup, indefinite retention, pause/restart, auto-retry, on-leave and presenter-exit cleanup")
        return 0
    finally:
        if process:
            (root / "terminal.bin").write_bytes(process.bytes())
            if process.child.poll() is None:
                process.send(b"q")
                try:
                    process.child.wait(timeout=40)
                except HELPERS.subprocess.TimeoutExpired:
                    pass
            process.stop()
        server.shutdown()
        server.server_close()
        thread.join(5)


if __name__ == "__main__":
    raise SystemExit(main())
