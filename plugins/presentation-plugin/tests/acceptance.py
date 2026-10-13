#!/usr/bin/env python3
"""Real staged-product acceptance, not a source-presence or unit-test substitute.

Run via scripts/smoke-presentation.sh --binary /absolute/stage/bmux
--plugins /absolute/stage/plugins. Evidence and isolated state are always retained.
Only loopback TLS and caller-owned BMUX/recorded-process runtimes are used.
Requires Python 3.11+, OpenSSL, compatible staged plugins and recorded-process
schema v4. Uses standalone protocol v2 and the product's managed view-v2 clients.
Retained private evidence contains test credentials/notes; no build/install occurs.
Use a short --state-parent (for example /hdd/bp-test). Cold recorded admission
hashes the staged executable and boots its dedicated server; --start-timeout
bounds that work separately from --command-timeout and --cleanup-timeout.
Interrupted admissions retain their exact launch key/argv and are reported as
unresolved, never silently retried with a fresh key or claimed cleaned up.
Owner-effect response loss is covered separately by the real TLS integration test
packages/presentation-runtime/tests/effect_network.rs, not by navigation retries.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import signal
import socket
import ssl
import struct
import subprocess
import tempfile
import time
import uuid

SECRET = "PRESENTATION_PRIVATE_NOTES_7349"
FIRST = "ACCEPTANCE_FIRST_7349"
LAST = "ACCEPTANCE_LAST_7349"
RELOADED = "ACCEPTANCE_RELOADED_7349"
DECK = f"""<!-- bmux:slide-id first -->
# {FIRST}

<!-- bmux:notes -->
{SECRET}
<!-- bmux:end-notes -->

---
<!-- bmux:slide-id last -->
# {LAST}
"""


def require(condition, message):
    if not condition:
        raise AssertionError(message)


class Wire:
    """Private acceptance probe of the standalone runtime's length-prefixed JSON.

    This is NOT a BMUX service adapter; managed coverage uses the real CLI and its
    generated clients. Socket deadlines and frame limits bound every observation.
    """
    def __init__(self, address, cert, credential, mode):
        context = ssl.create_default_context(cafile=str(cert))
        raw = socket.create_connection(address, timeout=5)
        try:
            self.socket = context.wrap_socket(raw, server_hostname="localhost")
        except BaseException:
            raw.close()
            raise
        self.socket.settimeout(5)
        auth = self.request({"Authenticate": {
            "version": 2, "credential": credential, "mode": mode}})
        require("Authenticated" in auth, f"authentication failed: {auth}")
        self.auth = auth["Authenticated"]

    def close(self):
        self.socket.close()

    def read_exact(self, length):
        result = bytearray()
        while len(result) < length:
            part = self.socket.recv(length - len(result))
            require(part, "TLS peer closed mid-frame")
            result.extend(part)
        return bytes(result)

    def request(self, value):
        body = json.dumps(value).encode()
        self.socket.sendall(struct.pack("!I", len(body)) + body)
        length = struct.unpack("!I", self.read_exact(4))[0]
        require(0 < length <= 16 * 1024 * 1024, "response exceeds acceptance frame budget")
        return json.loads(self.read_exact(length))

    def snapshot(self):
        return self.request("Snapshot")["Poll"]["Snapshot"]["snapshot"]

    def submit(self, action_id, snapshot, action):
        return self.request({"Submit": {"action_id": action_id,
            "expected": snapshot["cursor"], "action": action}})


class Acceptance:
    def __init__(self, args):
        self.binary = Path(args.binary).resolve(strict=True)
        require(self.binary.is_file() and os.access(self.binary, os.X_OK), "binary not executable")
        self.plugins = Path(args.plugins).resolve(strict=True)
        require(self.plugins.is_dir(), "plugin path must be a staged directory")
        self.root = Path(tempfile.mkdtemp(prefix="bp-", dir=args.state_parent))
        self.start_timeout = args.start_timeout
        self.command_timeout = args.command_timeout
        self.cleanup_timeout = args.cleanup_timeout
        self.cleanup_deadline = None
        # A dedicated server adds /tmp/r-<24 hex>/runtime/server.sock. Fail
        # before any service admission if its Unix address cannot fit. Use a short
        # --state-parent rather than leaving this to an expensive launch failure.
        socket_path = self.root / "tmp" / ("r-" + "0" * 24) / "runtime" / "server.sock"
        require(len(os.fsencode(socket_path)) < 104,
                "Unix socket path too long; select a shorter owned --state-parent")
        print(f"evidence/state retained: {self.root}", flush=True)
        self.env = {k: v for k, v in os.environ.items()
                    if k in ("PATH", "LANG", "LC_ALL", "TZ")}
        self.env.setdefault("PATH", "/usr/bin:/bin")
        for name in ("home", "config", "runtime", "data", "state", "logs", "tmp", "cache"):
            (self.root / name).mkdir(mode=0o700)
        self.env.update({"HOME": str(self.root / "home"), "TMPDIR": str(self.root / "tmp"),
            "XDG_CONFIG_HOME": str(self.root / "config"), "XDG_RUNTIME_DIR": str(self.root / "runtime"),
            "XDG_DATA_HOME": str(self.root / "data"), "XDG_STATE_HOME": str(self.root / "state"),
            "XDG_CACHE_HOME": str(self.root / "cache"), "BMUX_NO_BASE_CONFIG": "1",
            "BMUX_RUNTIME_NAME": "default", "BMUX_TARGET": "local", "TERM": "xterm-256color"})
        for name in ("config", "runtime", "data", "state", "logs"):
            variable = "LOG" if name == "logs" else name.upper()
            self.env[f"BMUX_{variable}_DIR"] = str(self.root / name)
        self.env["BMUX_RECORDINGS_DIR"] = str(self.root / "state" / "recordings")
        (self.root / "config" / "bmux.toml").write_text(
            '[plugins]\nenabled = ["bmux.presentation"]\nsearch_paths = '
            + json.dumps([str(self.plugins)]) + '\n')
        self.command = [str(self.binary), "--runtime", "default", "--target", "local",
                        "--config", str(self.root / "config" / "bmux.toml")]
        self.deck = self.root / "deck.md"
        self.deck.write_text(DECK)
        self.presenter = uuid.uuid4().hex + uuid.uuid4().hex
        self.audience = uuid.uuid4().hex + uuid.uuid4().hex
        for name, value in (("presenter", self.presenter), ("audience", self.audience)):
            path = self.root / name
            path.write_text(value)
            path.chmod(0o600)
        self.runs = []
        self.pending_starts = {}
        self.processes = []
        self.wires = []
        self.results = []
        self.sequence = 0
        self.server_started = False
        with self.binary.open("rb") as executable:
            binary_hash = hashlib.file_digest(executable, "sha256").hexdigest()
        self.save("identity", {"binary": str(self.binary),
            "sha256": binary_hash,
            "plugins": str(self.plugins)})

    def save(self, label, value):
        (self.root / f"{label}.json").write_text(json.dumps(value, indent=2))

    def cli(self, *args, as_json=False, deadline=None):
        self.sequence += 1
        sequence = self.sequence
        limit = self.command_timeout if deadline is None else deadline
        if self.cleanup_deadline is not None:
            limit = min(limit, self.cleanup_deadline - time.monotonic())
        require(limit > 0, "cleanup deadline exhausted; retained resources need recovery")
        started = time.monotonic()
        stdout_path = self.root / f"cli-{sequence}.stdout"
        stderr_path = self.root / f"cli-{sequence}.stderr"
        child = None
        failure = None
        # File-backed capture retains diagnostics even when admission is interrupted;
        # daemon descendants cannot keep communicate() pipe drains wedged.
        with stdout_path.open("wb") as stdout, stderr_path.open("wb") as stderr:
            try:
                child = subprocess.Popen([*self.command, *map(str, args)], env=self.env,
                    stdout=stdout, stderr=stderr, start_new_session=True)
                child.wait(timeout=limit)
            except BaseException as error:
                failure = type(error).__name__
                if child is not None and child.poll() is None:
                    os.killpg(child.pid, signal.SIGKILL)
                    child.wait(timeout=5)
                raise RuntimeError(f"CLI interrupted ({failure}) after {time.monotonic() - started:.1f}s; "
                                   f"see cli-{sequence}.stderr/stdout; outcome may be unresolved") from None
            finally:
                self.save(f"cli-{sequence}-observation", {"deadline_seconds": limit,
                    "elapsed_seconds": time.monotonic() - started, "interrupted": failure,
                    "returncode": None if child is None else child.returncode})
        output = stdout_path.read_bytes()
        (self.root / f"cli-{sequence}.log").write_bytes(output + stderr_path.read_bytes())
        require(child.returncode == 0,
                f"CLI failed ({child.returncode}); see cli-{sequence}.log")
        return json.loads(output) if as_json else output.decode()

    def spawn(self, label, *args):
        log = open(self.root / f"{label}.log", "wb")
        try:
            child = subprocess.Popen([*self.command, *map(str, args)], env=self.env,
                stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
        finally:
            log.close()
        self.processes.append(child)
        return child

    def await_log(self, label, pattern, child):
        deadline = time.monotonic() + 15
        while time.monotonic() < deadline:
            text = (self.root / f"{label}.log").read_text(errors="replace")
            match = re.search(pattern, text)
            if match:
                return match
            require(child.poll() is None, f"{label} exited before readiness; see log")
            time.sleep(0.1)
        raise AssertionError(f"{label} readiness timed out")

    def start_view(self, label, argv, size="100x30"):
        # Recorded children must explicitly route to the test's product runtime,
        # NOT the dedicated capture server's runtime or an inherited user target.
        # env -i also removes slot/attachment variables introduced by the capture
        # server. Supplying only --env overrides would leave those selectors alive.
        clean_environment = [f"{key}={value}" for key, value in self.env.items()]
        key = str(uuid.uuid4())
        arguments = ["recording", "process", "start", "--json", "--profile", "functional",
            "--viewport", size, "--idempotency-key", key, "--cwd", str(self.root),
            "--", "/usr/bin/env", "-i", *clean_environment, *self.command, *argv]
        pending = {"label": label, "launch_mutation_id": key,
                   "command": [*self.command, *arguments], "recovery_required": True}
        self.pending_starts[key] = pending
        self.save(f"{label}-admission", pending)
        status = self.cli(*arguments, as_json=True, deadline=self.start_timeout)
        run = status["run_id"]
        # Register the authoritative identity BEFORE validating other fields, so
        # later assertion failures cannot orphan an admitted recorded child.
        self.runs.append((label, run))
        del self.pending_starts[key]
        self.save(f"{label}-admission", dict(pending, run_id=run, recovery_required=False))
        require(status["schema_version"] == 4, "unsupported recorded-process contract")
        require(Path(status["runtime_root"]).resolve().is_relative_to(self.root),
                "recording runtime escaped owned root")
        self.save(f"{label}-start", status)
        return run

    def screen(self, run):
        return self.cli("recording", "process", "screen", run, "--json", as_json=True)["text"]

    def wait_screen(self, run, marker):
        try:
            result = self.cli("recording", "process", "wait", run, "--screen-contains", marker,
                              "--timeout", "10s", "--json", as_json=True)
        except Exception:
            # Preserve actual child diagnostics before cleanup/finalization. A
            # missing marker must not hide early CLI failures behind a wait timeout.
            for operation, options in (("status", []), ("screen", []),
                                       ("read", ["--max-bytes", "4096"])):
                try:
                    observed = self.cli("recording", "process", operation, run,
                                        *options, "--json", as_json=True)
                    self.save(f"{run}-failed-wait-{operation}", observed)
                except Exception as diagnostic_error:
                    self.save(f"{run}-failed-wait-{operation}",
                              {"observation_error": str(diagnostic_error)})
            raise
        require(result["matched"], f"screen did not contain {marker}")
        text = self.screen(run)
        require(marker in text, "screen wait/projection disagree")
        return text

    def send(self, run, text):
        self.cli("recording", "process", "send", run, "--text", text,
                 "--idempotency-key", str(uuid.uuid4()), "--json", as_json=True)

    def finish(self, label, run):
        self.send(run, "q")
        status = self.cli("recording", "process", "wait", run, "--exit", "--timeout", "10s",
                          "--json", as_json=True)
        require(status["target_exited"] and status["exit_code"] == 0, "viewer did not exit cleanly")
        result = self.cli("recording", "process", "finish", run, "--cleanup-server", "--json",
            "--export", "text", "--export", "raw", "--export", "events",
            "--output", self.root / f"{label}-export", as_json=True, deadline=self.start_timeout)
        require(result["status"]["durable_recording_complete"], "capture not durable")
        require(result["status"]["cleanup_complete"], "dedicated recording server cleanup incomplete")
        raw = (self.root / f"{label}-export" / "output.pty").read_bytes()
        require(raw, "empty PTY artifact")
        if label not in ("play-presenter", "managed-presenter"):
            require(SECRET.encode() not in raw, f"{label} raw PTY output leaked notes")
        self.save(f"{label}-finish", result)
        self.runs.remove((label, run))

    def passed(self, label):
        self.results.append(label)
        print(f"PASS {label}", flush=True)

    def connect(self, address, credential, mode):
        wire = Wire(address, self.root / "cert.pem", credential, mode)
        self.wires.append(wire)
        return wire

    def tls_contract(self, address):
        public = self.connect(address, None, "ReceiveOnly")
        audience = self.connect(address, self.audience, "Shared")
        presenter = self.connect(address, self.presenter, "Shared")
        independent = self.connect(address, self.presenter, "Independent")
        submit = self.connect(address, self.audience, "SubmitOnly")
        before = audience.snapshot()
        require(SECRET not in json.dumps(before), "notes leaked to private audience")
        require(SECRET not in json.dumps(public.snapshot()), "notes leaked to public audience")
        require(presenter.snapshot()["notes"].strip() == SECRET, "presenter notes missing")
        require(submit.snapshot()["state"] == before["state"], "submit-only bootstrap missing")
        require(submit.request({"Poll": {"cursor": None}}) == {"Error": "Unauthorized"},
                "submit-only acquired receive/poll permission")
        denied = public.submit(1, public.snapshot(), {"Navigate": "Last"})
        require(denied == {"Error": "Unauthorized"}, f"public write accepted: {denied}")
        self.passed("TLS public read-only, audience privacy, presenter notes, submit-only bootstrap")
        old = before["cursor"]
        ack = audience.submit(audience.auth["next_action_id"], before, {"Navigate": "Last"})
        require("Acknowledged" in ack, f"shared navigation rejected: {ack}")
        require(LAST in json.dumps(public.snapshot()), "shared content missing")
        require(public.snapshot()["state"]["slide_id"] == "last", "shared change not visible")
        require(independent.snapshot()["state"]["slide_id"] == "first", "independent followed shared")
        independent_ack = independent.submit(independent.auth["next_action_id"],
            independent.snapshot(), {"Navigate": "Last"})
        require("Acknowledged" in independent_ack, "independent navigation rejected")
        # Another principal changes shared state while the slow reader does not poll.
        fresh = presenter.snapshot()
        require("Acknowledged" in presenter.submit(
            independent_ack["Acknowledged"]["action_id"] + 1, fresh, {"Navigate": "First"}),
            "presenter principal sequence not shared across modes")
        repair = public.request({"Poll": {"cursor": old}})
        require("Snapshot" in repair["Poll"], "slow reader did not receive replacement snapshot")
        self.save("tls-slow-resync", repair)
        wrong_run = dict(old, run_id="different-host-run")
        replacement = public.request({"Poll": {"cursor": wrong_run}})
        require(replacement["Poll"]["Snapshot"]["resync"], "run mismatch did not require full resync")
        self.passed("TLS shared participants, independent navigation, slow-reader snapshot repair")
        # Disconnect after sending, before reading ACK: recovery resends the SAME
        # identity/payload, never a new action. This tests navigation, not owner effects.
        pending = {"Submit": {"action_id": ack["Acknowledged"]["action_id"] + 1,
            "expected": audience.snapshot()["cursor"], "action": {"Navigate": "Last"}}}
        body = json.dumps(pending).encode()
        audience.socket.sendall(struct.pack("!I", len(body)) + body)
        audience.close()
        resumed = self.connect(address, self.audience, "Shared")
        outcome = resumed.request(pending)
        require("Acknowledged" in outcome, f"uncertain navigation retry failed: {outcome}")
        require(resumed.request(pending) == outcome, "duplicate changed logical acknowledgment")
        require(resumed.snapshot()["cursor"] == outcome["Acknowledged"]["cursor"],
                "duplicate replay advanced state")
        conflict = json.loads(json.dumps(pending))
        conflict["Submit"]["action"] = {"Navigate": "First"}
        require(resumed.request(conflict) == {"Error": "ActionConflict"}, "conflicting reuse accepted")
        self.passed("TLS uncertain reconnect, exact navigation retry, conflicting identity rejection")
        action_id = outcome["Acknowledged"]["action_id"] + 1
        for navigation in ("First", "Last"):
            require("Acknowledged" in submit.submit(action_id, submit.snapshot(), {"Navigate": navigation}),
                    "submit-only navigation did not commit")
            require(public.snapshot()["state"]["slide_id"] == navigation.lower(),
                    "submit-only action not seen by shared receiver")
            action_id += 1
        self.passed("TLS submit-only authoritative navigation and poll denial")
        denied_effect = resumed.submit(action_id, resumed.snapshot(),
            {"Effect": {"name": "must-not-run", "input": "untrusted"}})
        require(denied_effect == {"Error": "Unauthorized"}, "audience acquired owner effects")
        self.passed("TLS audience effect authorization")

    def fixed_canvas(self):
        # Serialized data uses the public Deck schema; no Rust/source compilation.
        blocks = [{"id": identifier, "reveal_step": 0, "source": None,
                   "kind": {"type": "custom", "name": component, "config": {}}}
                  for identifier, component in (("count", "counter"), ("switch", "toggle"))]
        source = self.root / "controls.json"
        source.write_text(json.dumps({"id": "generic-controls",
            "metadata": {"title": "GENERIC_CONTROLS_7349", "author": None, "theme": None, "extra": {}},
            "slides": [{"id": "controls", "blocks": blocks, "reveal_steps": 0}]}))
        self.cli("present", "check", source)
        host = self.spawn("controls", "present", "host", source, "--listen", "127.0.0.1:0",
            "--cert", self.root / "cert.pem", "--key", self.root / "key.pem",
            "--presenter-credential", self.root / "presenter", "--audience-credential",
            self.root / "audience", "--public-read")
        port = int(self.await_log("controls", r"tls://127\.0\.0\.1:(\d+)", host)[1])
        writer = self.connect(("127.0.0.1", port), self.audience, "Shared")
        reader = self.connect(("127.0.0.1", port), None, "ReceiveOnly")
        independent = self.connect(("127.0.0.1", port), self.presenter, "Independent")
        initial = writer.snapshot()
        require({frame["surface_id"] for frame in initial["surfaces"]} == {"count", "switch"},
                "generic counter/toggle surfaces missing")
        require(all((frame["width"], frame["height"]) == (80, 24) for frame in initial["surfaces"]),
                "owner canvas not fixed at 80x24")
        isolated = independent.snapshot()["surfaces"]
        next_id = writer.auth["next_action_id"]
        for identifier, key in (("count", "+"), ("switch", " ")):
            before = writer.snapshot()
            frame = next(frame for frame in before["surfaces"] if frame["surface_id"] == identifier)
            invalid = {"DemoInput": {"surface_id": identifier, "surface_revision": frame["revision"],
                "x": 80, "y": 0, "input": key}}
            require(writer.submit(next_id, before, invalid) == {"Error": "Stale"},
                    "outside-canvas coordinate was not rejected")
            # Rejected fresh input consumes its action identity too.
            next_id += 1
            valid = {"DemoInput": {"surface_id": identifier, "surface_revision": frame["revision"],
                "x": 0, "y": 0, "input": key}}
            ack = writer.submit(next_id, before, valid)
            require("Acknowledged" in ack, f"generic component input failed: {ack}")
            after = reader.snapshot()
            changed = next(frame for frame in after["surfaces"] if frame["surface_id"] == identifier)
            require(changed["cells"] != frame["cells"], "owner input had no mirrored visible effect")
            require(independent.snapshot()["surfaces"] == isolated, "shared input changed independent scene")
            require(writer.submit(next_id, before, valid) == ack, "demo input duplicate changed outcome")
            require(reader.snapshot()["surfaces"] == after["surfaces"], "duplicate demo input replayed effect")
            next_id += 1
        self.save("controls-authoritative", reader.snapshot())
        input_run = self.start_view("controls-input", ["present", "join", f"tls://127.0.0.1:{port}",
            "--server-name", "localhost",
            "--ca", str(self.root / "cert.pem"), "--credential", str(self.root / "audience"),
            "--mode", "shared"], "100x30")
        self.wait_screen(input_run, "Counter: 1")
        self.send(input_run, "+")
        self.wait_screen(input_run, "Counter: 2")
        require(any("Counter: 2" in "".join(cell["symbol"] for cell in frame["cells"])
                    for frame in reader.snapshot()["surfaces"]), "PTY input did not reach owner")
        self.finish("controls-input", input_run)
        for label, size in (("controls-small", "80x24"), ("controls-large", "120x40")):
            run = self.start_view(label, ["present", "join", f"tls://127.0.0.1:{port}",
                "--server-name", "localhost",
                "--ca", str(self.root / "cert.pem")], size)
            self.wait_screen(run, "Counter: 2")
            self.wait_screen(run, "Toggle: true")
            self.finish(label, run)
        self.passed("generic counter/toggle shared mirrored effects, fixed canvas bounds, duplicate input, independent scene; two PTY sizes")

    def run(self):
        # Inspect supported help rather than assume a hidden testing command.
        for operation in ("play", "check", "host", "join"):
            self.cli("present", operation, "--help")
        self.cli("recording", "process", "start", "--help")
        self.cli("server", "start", "--help")
        self.server_started = True
        self.cli("server", "start", "--daemon", deadline=self.start_timeout)
        self.cli("present", "check", self.deck)
        play = self.start_view("play-audience", ["present", "play", str(self.deck)], "80x24")
        require(SECRET not in self.wait_screen(play, FIRST), "local audience notes leaked")
        self.send(play, "\x1b[F")
        self.wait_screen(play, LAST)
        self.finish("play-audience", play)
        notes = self.start_view("play-presenter", ["present", "play", str(self.deck), "--presenter"], "120x40")
        self.wait_screen(notes, SECRET)
        self.finish("play-presenter", notes)
        self.passed("check/play real PTY, navigation, two terminal sizes, local notes/privacy")
        # Explicit end-entity constraints override OpenSSL's default CA:true.
        # The self-signed leaf is trusted only by this test's explicit --ca file;
        # rustls still validates serverAuth, SAN and the peer signature.
        subprocess.run(["openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "1",
            "-subj", "/CN=localhost", "-addext", "subjectAltName=DNS:localhost,IP:127.0.0.1",
            "-addext", "basicConstraints=critical,CA:FALSE",
            "-addext", "keyUsage=critical,digitalSignature,keyEncipherment",
            "-addext", "extendedKeyUsage=serverAuth",
            "-keyout", str(self.root / "key.pem"), "-out", str(self.root / "cert.pem")],
            env=self.env, check=True, timeout=20, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        (self.root / "key.pem").chmod(0o600)
        host = self.spawn("tls", "present", "host", self.deck, "--listen", "127.0.0.1:0",
            "--cert", self.root / "cert.pem", "--key", self.root / "key.pem",
            "--presenter-credential", self.root / "presenter", "--audience-credential",
            self.root / "audience", "--public-read")
        port = int(self.await_log("tls", r"tls://127\.0\.0\.1:(\d+)", host)[1])
        untrusted = ssl.create_default_context()
        try:
            with socket.create_connection(("127.0.0.1", port), timeout=5) as raw:
                with untrusted.wrap_socket(raw, server_hostname="localhost"):
                    raise AssertionError("untrusted presentation TLS certificate accepted")
        except ssl.SSLCertVerificationError:
            self.passed("TLS explicit trust required; untrusted certificate rejected")
        self.tls_contract(("127.0.0.1", port))
        self.fixed_canvas()
        managed = self.cli("present", "host", self.deck, "--managed", "--presenter-credential",
            self.root / "presenter", "--audience-credential", self.root / "audience", "--public-read")
        match = re.search(r"bmux:[^\s]+", managed)
        require(match is not None, "managed host did not return endpoint")
        for transport, endpoint in (("tls", f"tls://127.0.0.1:{port}"), ("managed", match[0])):
            for index, mode in enumerate(("receive-only", "submit-only", "shared", "independent")):
                label = f"{transport}-{mode}"
                argv = ["present", "join", endpoint, "--mode", mode]
                if transport == "tls":
                    argv += ["--ca", str(self.root / "cert.pem"), "--server-name", "localhost"]
                if mode != "receive-only":
                    argv += ["--credential", str(self.root / "audience")]
                run = self.start_view(label, argv, ("80x24", "120x40")[index % 2])
                # Submit-only still has a bootstrap view, but no poll permission.
                text = self.wait_screen(run, FIRST if transport == "managed" else LAST)
                require(SECRET not in text, f"{label} leaked notes")
                self.finish(label, run)
            self.passed(f"{transport} join real PTY, all four modes, sizes/privacy")
        owner_notes = self.start_view("managed-presenter", ["present", "join", match[0],
            "--credential", str(self.root / "presenter"), "--presenter"], "120x40")
        self.wait_screen(owner_notes, SECRET)
        self.finish("managed-presenter", owner_notes)
        read_run = self.start_view("managed-live-reader", ["present", "join", match[0]], "80x24")
        write_run = self.start_view("managed-live-writer", ["present", "join", match[0],
            "--credential", str(self.root / "audience"), "--mode", "shared"], "120x40")
        independent_run = self.start_view("managed-live-independent", ["present", "join", match[0],
            "--credential", str(self.root / "presenter"), "--mode", "independent"], "100x30")
        for run in (read_run, write_run, independent_run):
            self.wait_screen(run, FIRST)
        self.send(write_run, "\x1b[F")
        self.wait_screen(write_run, LAST)
        require(SECRET not in self.wait_screen(read_run, LAST), "managed update leaked notes")
        require(FIRST in self.screen(independent_run), "managed independent viewer followed shared")
        self.send(write_run, "\x1b[H")
        self.wait_screen(read_run, FIRST)
        # Public navigation cannot change the owner. Observe the private writer too.
        self.send(read_run, "\x1b[F")
        time.sleep(0.5)
        require(FIRST in self.screen(write_run), "managed public reader changed owner navigation")
        for label, run in (("managed-live-reader", read_run), ("managed-live-writer", write_run),
                           ("managed-live-independent", independent_run)):
            self.finish(label, run)
        self.passed("managed simultaneous participants, shared navigation, independent view, public read-only, authorized presenter notes")
        # Invalid/failed preparation must retain the last valid deck for connected viewers.
        watcher = self.start_view("managed-watch", ["present", "join", match[0]], "100x30")
        self.wait_screen(watcher, FIRST)
        public = self.connect(("127.0.0.1", port), None, "ReceiveOnly")
        retained_cursor = public.snapshot()["cursor"]
        for invalid in ("<!-- bmux:notes -->\nunterminated", "# broken\n\n![bad](missing.png)\n",
                        "# oversized\n" + "x" * (8 * 1024 * 1024 + 1)):
            self.deck.write_text(invalid)
            time.sleep(1.2)  # exceed documented watcher cadence, bounded by whole-run timeout
            require(FIRST in self.screen(watcher), "failed reload discarded last valid deck")
            require(public.snapshot()["cursor"] == retained_cursor,
                    "failed TLS reload advanced the committed deck cursor")
        self.deck.write_text(DECK.replace(FIRST, RELOADED))
        self.wait_screen(watcher, RELOADED)
        self.finish("managed-watch", watcher)
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline and RELOADED not in json.dumps(public.snapshot()):
            time.sleep(0.2)
        require(RELOADED in json.dumps(public.snapshot()), "TLS valid watch reload not installed")
        self.passed("managed invalid/failed/oversized preparation retention and valid watch reload; TLS valid reload")

    def cleanup(self):
        self.cleanup_deadline = time.monotonic() + self.cleanup_timeout
        errors = []
        # Admission can be interrupted before any run ID exists. Never infer a
        # run from its launch key, bypass the CLI's ambiguous-launch guard, or
        # silently report cleanup success for an unregistered dedicated runtime.
        for pending in self.pending_starts.values():
            errors.append(f"{pending['label']}: unresolved launch {pending['launch_mutation_id']}; "
                          "retain admission record and owned TMPDIR for coordinator recovery; no retry")
        for wire in self.wires:
            wire.close()
        for label, run in list(self.runs):
            steps = [
                ("terminate", ["terminate", run, "--grace-period", "2s", "--json"]),
                ("wait", ["wait", run, "--exit", "--timeout", "10s", "--json"]),
                ("finish", ["finish", run, "--cleanup-server", "--json"]),
            ]
            for step, arguments in steps:
                try:
                    result = self.cli("recording", "process", *arguments, as_json=True,
                        deadline=self.start_timeout if step == "finish" else self.command_timeout)
                    self.save(f"{label}-cleanup-{step}", result)
                    if step == "finish":
                        require(result["durable_recording_complete"] and result["cleanup_complete"],
                                "finalization/dedicated cleanup incomplete")
                except Exception as error:
                    errors.append(f"{label} {step}: {error}")
        for child in reversed(self.processes):
            if child.poll() is None:
                try:
                    os.killpg(child.pid, signal.SIGTERM)
                    remaining = max(0.01, min(5, self.cleanup_deadline - time.monotonic()))
                    child.wait(timeout=remaining)
                except subprocess.TimeoutExpired:
                    os.killpg(child.pid, signal.SIGKILL)
                    child.wait(timeout=5)
                except ProcessLookupError:
                    child.wait(timeout=5)
        if self.server_started:
            try:
                self.cli("server", "stop")
            except Exception as error:
                errors.append(f"owned product server: {error}")
        self.save("coverage", {"passed": self.results, "cleanup_errors": errors,
            "unresolved_admissions": list(self.pending_starts.values()),
            "remaining_dedicated_roots": [str(path) for path in (self.root / "tmp").glob("r-*")],
            "separate_integration_test": "packages/presentation-runtime/tests/effect_network.rs",
            "gaps": ["CLI owner rejects effects; run the separate TLS effect-network test",
                     "counter/toggle geometry covered by authoritative TLS and PTY mirrors; managed demo input still untested",
                     "oversized source covered; rich-asset worker timeout/oversized scene remains a gap",
                     "managed wire parity exercised by separate service projection unit tests"]})
        require(not errors, f"cleanup failed: {errors}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True, help="explicit staged executable (never PATH)")
    parser.add_argument("--plugins", required=True, help="compatible staged plugins, no installation")
    parser.add_argument("--timeout", type=int, default=600,
                        help="scenario deadline after initialization (seconds); cleanup separate")
    parser.add_argument("--state-parent", help="short existing test-owned parent (default: TMPDIR)")
    parser.add_argument("--start-timeout", type=int, default=120,
                        help="bounded cold admission budget, including executable hashing (seconds)")
    parser.add_argument("--command-timeout", type=int, default=45,
                        help="other CLI budget; exceeds recorded service's 35s transport deadline")
    parser.add_argument("--cleanup-timeout", type=int, default=180,
                        help="total cleanup CLI budget (seconds); owned process reaps add up to 5s each")
    args = parser.parse_args()
    require(all(value > 0 for value in (args.timeout, args.start_timeout,
                args.command_timeout, args.cleanup_timeout)), "timeouts must be positive")
    def interrupted(signum, _frame):
        raise TimeoutError(f"acceptance interrupted by signal {signum}")
    signal.signal(signal.SIGALRM, interrupted)
    signal.signal(signal.SIGTERM, interrupted)
    acceptance = Acceptance(args)
    signal.alarm(args.timeout)
    try:
        acceptance.run()
    except BaseException as error:
        acceptance.save("failure", {"type": type(error).__name__, "message": str(error)})
        raise
    finally:
        signal.alarm(0)
        acceptance.cleanup()
    print("presentation acceptance passed; see retained coverage.json and recordings")


if __name__ == "__main__":
    main()
