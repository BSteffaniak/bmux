#!/usr/bin/env python3
"""Owned Linux presenter acceptance; explicit prebuilt artifacts, no builds/install.

Imports bounded PTY, HTTP, CDP and Canvas helpers without running their suites.
All commands share an isolated default/local configuration. Only production may
create the lazy presenter service. No user daemon or configuration is inspected.
"""
import argparse
import contextlib
import importlib.util
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import sys
import threading
import time

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parent.parent
SPEC = importlib.util.spec_from_file_location("presenter_acceptance_helpers", ROOT / "scripts/presenter-web-acceptance.py")
HELPERS = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(HELPERS)
WEB = HELPERS.WEB
require, wait_for = WEB.require, WEB.wait_for
MARKER = "OWNED_PRESENTER_REAL_SHELL"
TITLE = "Actual BMUX session"
RELOADED = "Reloaded live session"


def identity(pid):
    try:
        fields = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
        return {"pid": int(pid), "start": fields[19], "parent": int(fields[1]), "state": fields[0]}
    except (OSError, IndexError, ValueError):
        return None


def alive(record):
    current = identity(record["pid"])
    return current and current["start"] == record["start"] and current["state"] != "Z"


def bounded_json(path):
    require(path.stat().st_size <= 16384, "oversized owned evidence")
    return json.loads(path.read_text())


class Acceptance(HELPERS.Acceptance):
    def __init__(self, args):
        # Reuse deterministic isolated paths and artifact hashing, suppress the
        # helper's progress chatter: this harness emits one safe final summary.
        with open(os.devnull, "w") as quiet, contextlib.redirect_stdout(quiet):
            super().__init__(args)
        self.env["SHELL"] = "/bin/sh"
        self.records = {}
        self.records_lock = threading.Lock()
        self.scan_lock = threading.Lock()
        self.tracking_stop = threading.Event()
        self.services = {}
        self.attempt_evidence = []
        self.tracker_error = None
        self.tracker = threading.Thread(target=self.track, daemon=True)
        self.deck = self.root / "live.md"
        self.original = (ROOT / "plugins/presenter-plugin/examples/live-demo.md").read_text()
        self.deck.write_text(self.original)
        self.source = self.root / "live-demo.dsl"
        # The fixed 80x24 surface is centrally clipped by the smaller notes
        # viewport. Put evidence in its shared visible center, not cropped row 0.
        self.approved = ('@name Owned regression\n@viewport cols=80 rows=24\n@shell sh\n@timeout 180000\n'
                         f'new-session\nsend-keys keys="printf \'\\\\033[12;1H{MARKER}\\\\n\'; sleep 120\\r"\n'
                         f'wait-for pattern="{MARKER}"\n'
                         'wait-for pattern="NEVER_COMPLETE_OWNED_REGRESSION" timeout=120000\n')
        self.source.write_text(self.approved)
        self.tracker.start()

    def write_config(self, extra=""):
        # Deliberately omit bmux.playbook on the unrelated/global server.
        self.config.write_text('[plugins]\ndisabled = ["bmux.playbook"]\nenabled = ["bmux.presenter", "bmux.sessions", '
                               '"bmux.contexts", "bmux.clients", "bmux.tabs"]\nsearch_paths = '
                               + json.dumps([str(self.args.plugins)]) + '\n' + extra)

    def passed(self, label):
        self.cases.append(label)

    def remember(self, record):
        key = (record["pid"], record["start"])
        try:
            record["bmux"] = Path(f'/proc/{record["pid"]}/exe').resolve(strict=True) == self.args.binary
        except OSError:
            record["bmux"] = False
        with self.records_lock:
            record["bmux"] = record["bmux"] or self.records.get(key, {}).get("bmux", False)
            self.records[key] = record
        return record

    def own(self, pid):
        record = identity(pid)
        return self.remember(record) if record else None

    def scan(self):
        with self.scan_lock:
            self.scan_owned()

    def scan_owned(self):
        # /proc stat contains identity/parentage only, not argv or credentials.
        table = [identity(p.name) for p in Path("/proc").iterdir() if p.name.isdigit()]
        table = [p for p in table if p]
        with self.records_lock:
            roots = {r["pid"] for r in self.records.values() if alive(r)}
        roots.update(p.child.pid for p in self.processes + self.browser_processes if p.child.poll() is None)
        changed = True
        while changed:
            changed = False
            for record in table:
                if record["pid"] in roots or record["parent"] in roots:
                    if record["pid"] not in roots:
                        roots.add(record["pid"])
                        changed = True
                    self.remember(record)
        # A server may daemonize between polls. Its owned pidfile and exact exe
        # establish ownership independently of the temporary parent relation.
        for pidfile in self.root.rglob("server.pid"):
            try:
                pid = int(pidfile.read_text().strip())
                if Path(f"/proc/{pid}/exe").resolve(strict=True) == self.args.binary:
                    record = self.own(pid)
                    if record:
                        with self.records_lock:
                            self.services[str(pidfile.parent)] = record
            except (OSError, ValueError):
                continue

    def track(self):
        while not self.tracking_stop.is_set():
            try:
                self.scan()
            except OSError:
                # Owned directories may disappear during successful shutdown.
                pass
            except Exception as error:
                self.tracker_error = type(error).__name__
                return
            self.tracking_stop.wait(0.05)

    def service_records(self):
        with self.records_lock:
            return dict(self.services)

    def service_roots(self):
        return sorted((self.root / "state/presenter-playbooks").glob("service-*"))

    def attempts(self):
        # No private-service root convention required; search only owned state.
        paths = sorted(self.root.rglob("runner.json"))
        require(len(paths) <= 16, "too many owned attempts")
        return paths

    def snapshot(self, url):
        origin, token = self.endpoint(url)
        status, body = WEB.http(origin, "/snapshot", token)
        require(status in (200, 503), "owned snapshot rejected")
        return json.loads(body) if status == 200 else None

    def frame(self, url, marker):
        return wait_for("shared retained frame", lambda: self.matching_frame(url, marker), 25)

    def matching_frame(self, url, marker):
        frame = self.snapshot(url)
        return frame if frame and marker in WEB.text(frame) else None

    def stable(self, label, predicate, seconds=1):
        # Negative assertions require a bounded observation window, never an
        # arbitrary sleep followed by a timing-based pass.
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            self.scan()
            require(predicate(), label)
            self.tracking_stop.wait(0.05)

    def bmux_processes(self):
        self.scan()
        with self.records_lock:
            records = list(self.records.values())
        result = []
        for record in records:
            try:
                if alive(record) and Path(f'/proc/{record["pid"]}/exe').resolve(strict=True) == self.args.binary:
                    result.append(record)
            except OSError:
                continue
        return result

    def unrelated_alive(self):
        require(self.global_server.child.poll() is None and alive(self.global_identity), "unrelated server changed identity")
        principal = self.root / "data/runtime/principal-id"
        require(principal.is_file() and WEB.digest(principal) == self.principal_hash, "unrelated server principal changed")
        require(int((self.root / "runtime/server.pid").read_text().strip()) == self.global_identity["pid"], "unrelated pidfile replaced")
        with socket.socket(socket.AF_UNIX) as connection:
            connection.settimeout(1)
            connection.connect(str(self.root / "runtime/server.sock"))
        return True

    def observed_bmux(self):
        self.scan()
        with self.records_lock:
            return {r["pid"] for r in self.records.values() if r.get("bmux")}

    def no_new_runtime(self, allowed):
        return (not list(self.root.rglob("runtime.json")) and not self.service_roots()
                and not self.attempts()
                and self.observed_bmux() <= allowed
                and all(Path(p) == self.root / "runtime" for p in self.service_records()))

    def frozen(self, attempt):
        files = sorted((attempt.parent / "sources").rglob("*"))
        require(len(files) <= 64, "oversized frozen source closure")
        result = {str(p.relative_to(attempt.parent)): WEB.digest(p) for p in files if p.is_file()}
        require(result, "missing frozen source evidence")
        return result

    def started(self, count):
        paths = self.attempts()
        if len(paths) != count:
            return None
        for path in paths:
            value = bounded_json(path)
            if value.get("session_id") and not value.get("cleanup_acknowledged"):
                return path
        return None

    def attempt_record(self, path):
        runner = bounded_json(path)
        runtime = bounded_json(path.parent / "runtime.json")
        directory = Path(runtime["runtime_dir"]).resolve()
        require(directory.is_relative_to(self.root), "attempt runtime escaped owned root")
        require(runtime["generation"] == runner["generation"], "attempt evidence generation mismatch")
        require((path.parent / "probe.json").read_bytes() == b"bmux-playbook-worker-v1", "worker protocol was not negotiated")
        return runner, directory

    def cleaned(self, path):
        value = bounded_json(path)
        directory = Path(bounded_json(path.parent / "runtime.json")["runtime_dir"])
        return value.get("cleanup_acknowledged") and not directory.exists()

    def run(self):
        self.global_server = self.spawn(["server", "start", "--foreground-internal", "--no-pane-shell-integration"])
        self.global_identity = self.own(self.global_server.child.pid)
        principal = self.root / "data/runtime/principal-id"
        wait_for("unrelated default server", lambda: (self.root / "runtime/server.sock").exists()
                 and (self.root / "runtime/server.pid").exists() and principal.exists()
                 and self.global_server.child.poll() is None, 20)
        self.principal_hash = WEB.digest(principal)
        self.unrelated_alive()
        check = self.spawn(["present", "check", self.deck])
        self.own(check.child.pid)
        require(check.child.wait(timeout=20) == 0, "deck check failed")
        initial = {self.global_identity["pid"], check.child.pid}
        self.stable("deck check launched execution", lambda: self.no_new_runtime(initial))
        argv = ["present", "play", self.deck, "--allow-playbooks", "--presenter", "--web",
                "--web-assets", self.args.assets, "--web-port", "0", "--web-auth", "none"]
        player = self.spawn(argv, mirror=True)
        self.own(player.child.pid)
        url = self.url(player)
        browser = self.browser(url)
        self.commit(browser, self.frame(url, TITLE))
        player.send(b"\t")
        player.send(b"o")
        wait_for("opened demo overlay", lambda: b"Run" in player.bytes() or b"run" in player.bytes())
        self.stable("deck open/overlay launched execution", lambda: self.no_new_runtime(
            initial | {player.child.pid}))
        self.passed("check/open/overlay are lazy")
        player.send(b"r")
        first = wait_for("first actual attempt", lambda: self.started(1), 25)
        frame = self.frame(url, MARKER)
        unavailable = frame.get("unsupported_metadata", [])
        regions = frame.get("metadata_availability", [])
        require(all(kind in unavailable or any(not region.get(kind, True) for region in regions)
                    for kind in ("selection", "semantics")),
                "embedded metadata availability missing")
        self.commit(browser, frame)
        wait_for("same live shell in controlling PTY", lambda: MARKER.encode() in player.bytes(), 15)
        before, first_runtime = self.attempt_record(first)
        frozen = self.frozen(first)
        wait_for("private service and attempt server", lambda: len(self.service_records()) >= 3, 15)
        service_roots = self.service_roots()
        require(len(service_roots) == 1, "expected exactly one automatic presenter service root")
        service_runtime = service_roots[0] / "runtime"
        private_services = {p: r for p, r in self.service_records().items() if Path(p) == service_runtime}
        require(private_services and (service_runtime / "server.sock").is_socket(),
                "no automatic private presenter service discovered")
        require(first_runtime != service_runtime and first_runtime != self.root / "runtime",
                "attempt did not use a separate isolated runtime")
        require(all(r and r["pid"] != self.global_identity["pid"] for r in private_services.values()), "private service reused unrelated server")
        self.unrelated_alive()
        self.passed("automatic private service, negotiated actual shell, shared Canvas frame")
        self.deck.write_text(self.original.replace(TITLE, RELOADED))
        self.commit(browser, self.frame(url, RELOADED))
        after = bounded_json(first)
        require(self.attempts() == [first] and after["generation"] == before["generation"]
                and after["session_id"] == before["session_id"] and not after["cleanup_acknowledged"]
                and self.frozen(first) == frozen, "compatible reload replaced active attempt/source")
        self.commit(browser, self.frame(url, MARKER))
        self.passed("compatible reload retains attempt, generation, session, frozen source and frame")
        self.source.write_text(self.approved + "# changed approved closure\n")
        self.deck.write_text(self.original.replace(TITLE, "UNAPPROVED replacement"))
        wait_for("explicit closure rejection", lambda: b"Reload rejected" in player.bytes()
                 and b"closure changed" in player.bytes(), 15)
        frame = self.frame(url, RELOADED)
        require("UNAPPROVED replacement" not in WEB.text(frame), "unapproved deck published")
        require(self.attempts() == [first] and not bounded_json(first)["cleanup_acknowledged"]
                and self.frozen(first) == frozen, "closure rejection stopped/replaced approved attempt")
        self.commit(browser, self.frame(url, MARKER))
        self.passed("changed approved closure rejected without retiring original")
        # Restore approved bytes before Restart; approval cannot be bypassed.
        self.source.write_text(self.approved)
        self.deck.write_text(self.original.replace(TITLE, RELOADED))
        player.send(b"s")
        wait_for("Stop acknowledged and attempt sockets removed", lambda: self.cleaned(first), 20)
        player.send(b"R")
        second = wait_for("Restart actual attempt", lambda: self.started(2), 25)
        restarted, _ = self.attempt_record(second)
        require(restarted["generation"] != before["generation"] and restarted["session_id"] != before["session_id"], "Restart reused generation/session")
        self.commit(browser, self.frame(url, MARKER))
        self.passed("Stop cleanup precedes fresh Restart generation")
        private_sockets = [p for p in self.root.rglob("*.sock")
                           if not p.is_relative_to(self.root / "runtime")]
        require(private_sockets, "missing private socket evidence")
        # q must clean an ACTIVE restarted attempt, not just an already-stopped one.
        player.send(b"q")
        wait_for("clean q exit", lambda: player.child.poll() is not None, 25)
        require(player.child.returncode == 0, "presenter q failed")
        wait_for("all attempts cleanup acknowledged", lambda: all(self.cleaned(p) for p in self.attempts()), 20)
        self.scan()
        private_services = {p: r for p, r in self.service_records().items() if Path(p) != self.root / "runtime"}
        wait_for("all private service sockets and PIDs removed", lambda: all(
            not alive(r) and not list(Path(p).glob("*.sock")) and not (Path(p) / "server.pid").exists()
            for p, r in private_services.items()), 20)
        wait_for("owned observer and service sockets gone", lambda: not any(p.exists() for p in private_sockets)
                 and not any(p for p in self.root.rglob("*.sock")
                             if not p.is_relative_to(self.root / "runtime")), 20)
        wait_for("q reaped owned bmux children", lambda:
                 {r["pid"] for r in self.bmux_processes()} == {self.global_identity["pid"]}, 15)
        self.unrelated_alive()
        self.attempt_evidence = [{"path": str(p.relative_to(self.root)),
                                  "generation": bounded_json(p)["generation"],
                                  "cleanup_acknowledged": True} for p in self.attempts()]
        self.passed("q cleans active attempts and private services; unrelated principal/server survives")
        # Managed host MUST use its selected host service, never the lazy local owner.
        managed_baseline = self.observed_bmux()
        managed = self.spawn(["present", "host", self.deck, "--managed", "--allow-playbooks",
                              "--presenter-credential", self.root / "presenter", "--public-read",
                              "--web", "--web-assets", self.args.assets, "--web-port", "0", "--web-auth", "none"], mirror=True)
        self.own(managed.child.pid)
        wait_for("managed missing-provider failure", lambda: managed.child.poll() is not None, 15)
        diagnostic = managed.bytes().lower()
        require(managed.child.returncode != 0 and b"playbook" in diagnostic
                and any(word in diagnostic for word in (b"missing", b"unavailable", b"not registered", b"not found", b"unknown service", b"not enabled")),
                "managed missing provider did not fail clearly")
        require(len(self.attempts()) == 2 and not self.service_roots(), "managed host secretly created local attempt/service")
        self.stable("managed missing provider launched hidden local service", lambda:
                    self.observed_bmux() <= managed_baseline | {managed.child.pid}
                    and {r["pid"] for r in self.bmux_processes()} == {self.global_identity["pid"]})
        self.unrelated_alive()
        self.passed("managed missing provider fails explicitly with no local fallback")

    def cleanup(self):
        failures = []
        self.scan()
        # Capture private bounded PTY evidence; never expose tokens/URLs in console.
        for process in self.processes:
            if isinstance(process, HELPERS.Mirror):
                (self.root / f"mirror-{process.child.pid}.pty").write_bytes(process.bytes())
        for client in self.clients:
            try:
                client.close()
            except OSError:
                failures.append("CDP cleanup")
        for process in reversed(self.browser_processes + self.processes):
            try:
                process.stop()
            except (OSError, subprocess.TimeoutExpired):
                failures.append("owned direct child cleanup")
        self.scan()
        self.tracking_stop.set()
        self.tracker.join(timeout=2)
        if self.tracker_error:
            failures.append("process identity tracker failed")
        with self.records_lock:
            records = list(self.records.values())
        # Failure cleanup is restricted to captured PID+start identities. Never
        # kill by basename, generic runtime name, or an unverified stale pidfile.
        for sig in (signal.SIGTERM, signal.SIGKILL):
            for record in records:
                if alive(record):
                    try:
                        os.kill(record["pid"], sig)
                    except ProcessLookupError:
                        pass
            try:
                wait_for("owned descendants reaped", lambda: not any(alive(r) for r in records), 5)
                break
            except RuntimeError:
                if sig == signal.SIGKILL:
                    failures.append("owned descendants still alive")
        report = {"status": "FAIL" if self.failed or failures else "PASS", "failure": self.failed,
                  "cleanup_failures": failures, "identity": self.identity, "passed_cases": self.cases,
                  "root": str(self.root), "processes": [{"pid": r["pid"], "start": r["start"]} for r in records],
                  "services": [{"runtime": p, "pid": r["pid"], "start": r["start"]} for p, r in self.service_records().items() if r],
                  "attempts": self.attempt_evidence}
        path = self.root / "report.json"
        path.write_text(json.dumps(report, indent=2))
        print(json.dumps({"status": report["status"], "evidence": str(path), "cases": len(self.cases),
                          "processes": report["processes"][:64], "process_count": len(records),
                          "services": report["services"],
                          "failure": self.failed, "cleanup_failures": failures}), flush=True)
        return not self.failed and not failures


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("binary", "plugins", "assets", "browser"):
        parser.add_argument("--" + name, type=lambda value: Path(value).resolve(strict=True), required=True)
    parser.add_argument("--mermaid-worker", type=lambda value: Path(value).resolve(strict=True), required=True)
    parser.add_argument("--state-parent", type=lambda value: Path(value).resolve(strict=True), default=Path("/tmp"))
    args = parser.parse_args()
    require(Path("/proc/self/stat").is_file(), "Linux /proc required")
    for name in ("binary", "browser"):
        require(getattr(args, name).is_file() and os.access(getattr(args, name), os.X_OK), "explicit executable required")
    require(args.plugins.is_dir() and args.assets.is_dir(), "explicit staged directories required")
    for name in WEB.ASSETS:
        require((args.assets / name).is_file(), "missing built browser asset")
    suite = Acceptance(args)
    try:
        suite.run()
    except BaseException as error:
        # Only fixed assertion labels are safe; foreign subprocess/CDP errors
        # may contain capability URLs, so store the exception type only.
        suite.failed = type(error).__name__
    return 0 if suite.cleanup() else 1


if __name__ == "__main__":
    try:
        sys.exit(main())
    except Exception as error:
        print(json.dumps({"status": "FAIL", "failure": type(error).__name__}), file=sys.stderr)
        sys.exit(1)
