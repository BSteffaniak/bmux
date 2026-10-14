#!/usr/bin/env python3
"""Real presenter player acceptance against explicit prebuilt artifacts (Linux).

Run only under the coordinator's runtime/browser resource lock. No builds, VT
parser, browser input, synthetic service, or user runtime. The opt-in product
hook BMUX_PRESENTER_FRAME_EVIDENCE atomically publishes successfully committed
terminal cells and clipped Button regions. Missing evidence is a failure, never
a fallback to escape-stream text or guessed coordinates. Private evidence and
owned state are retained; console diagnostics contain no capabilities or URLs.

--managed starts a DISTINCT owned provider-enabled host server, not a config
rewrite pretending to enable the unrelated disabled-provider server. The same
PTY click suite applies to both modes. Browser inspection is audience-only.
"""
import argparse
import base64
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parent.parent
SPEC = importlib.util.spec_from_file_location(
    "presenter_player_owned_helpers", ROOT / "scripts/presenter-owned-runtime-acceptance.py")
OWNED = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(OWNED)
HELPERS, WEB = OWNED.HELPERS, OWNED.WEB
require, wait_for = OWNED.require, OWNED.wait_for
TITLE = "Actual BMUX session"
RELOADED = "Reloaded player session"
SPEEDS = ("half", "normal", "double", "instant")
LABELS = {"speed:half": "Half", "speed:normal": "Normal",
          "speed:double": "Double", "speed:instant": "Instant",
          "step": "Step", "stop": "Stop", "restart": "Restart", "close": "Close"}


class Acceptance(OWNED.Acceptance):
    def __init__(self, args):
        super().__init__(args)
        self.evidence = self.root / "committed-terminal.json"
        self.env["BMUX_PRESENTER_FRAME_EVIDENCE"] = str(self.evidence)
        self.output = self.root / "shell-lines.txt"
        # Printable, unique payloads expose incremental real shell echo. A long
        # line makes Pause observable even on fast hosts; no product sleeps.
        self.payloads = ["PLAYER_FIRST_" + "0123456789ABCDEF" * 24,
                         "PLAYER_SECOND_" + "FEDCBA9876543210" * 24,
                         "PLAYER_STEP_" + "ABCDEF0123456789" * 16]
        commands = [f"printf '%s\\n' '{payload}' >> '{self.output}'"
                    for payload in self.payloads]
        self.approved = ('@name Player real acceptance\n@viewport cols=68 rows=12\n'
                         '@timeout 180000\nnew-session\n' + ''.join(
                             'send-keys keys=' + json.dumps(command + '\r') + '\n'
                             for command in commands) +
                         'wait-for pattern="PLAYER_NEVER_COMPLETE" timeout=120000\n')
        self.source.write_text(self.approved)
        # Navigating/hiding a reference must not cancel shared work.
        self.original = self.original.replace(',"on_leave":"stop"', '')
        self.deck.write_text(self.original)
        self.capture_sequence = 0
        self.last_click_revision = -1
        self.player = self.audience = None
        self.active = None

    def terminal(self):
        if not self.evidence.exists():
            return None
        require(self.evidence.stat().st_size <= 256 * 1024, "terminal evidence exceeds budget")
        frame = json.loads(self.evidence.read_text())
        require(isinstance(frame.get("revision"), int), "terminal revision missing")
        width, height = frame.get("width"), frame.get("height")
        require(isinstance(width, int) and isinstance(height, int)
                and 0 < width <= 512 and 0 < height <= 256, "terminal geometry invalid")
        require(len(frame.get("rows", [])) == height
                and all(isinstance(row, str) for row in frame["rows"]), "committed rows missing")
        require(isinstance(frame.get("controls"), list), "committed controls missing")
        return frame

    def control(self, input_name, label=None):
        frame = self.terminal()
        if not frame or frame["revision"] <= self.last_click_revision:
            return None
        choices = [control for control in frame["controls"]
                   if control["input"] == input_name and control["enabled"]]
        if not choices:
            return None
        require(len(choices) == 1, "ambiguous committed Button")
        control = choices[0]
        x, y, width, height = [control[key] for key in ("x", "y", "width", "height")]
        require(all(isinstance(value, int) for value in (x, y, width, height))
                and width > 0 and height > 0 and x >= 0 and y >= 0
                and x + width <= frame["width"] and y + height <= frame["height"],
                "Button region not clipped to committed cells")
        # Verify the label inside the SAME retained region used for clicking.
        visible = '\n'.join(row[x:x + width] for row in frame["rows"][y:y + height])
        expected = label or LABELS.get(input_name)
        if input_name == "play-toggle" and expected is None:
            require("Play" in visible or "Pause" in visible, "play Button label missing")
        else:
            if expected and expected not in visible:
                # State acknowledgements can precede the next terminal commit.
                # Keep waiting for the requested label in its real hit region.
                return None
            require(expected, "Button label and hit region disagree")
        require(frame.get("demo") and frame["demo"].get("id"), "modal ownership missing")
        if self.active:
            attempt = self.attempt()
            rows = '\n'.join(frame["rows"])
            paused = OWNED.bounded_json(self.active.parent / "control.json").get("paused", False)
            if bool(paused) != (" · paused" in rows):
                return None
            if attempt.get("current_action") and attempt["current_action"] not in rows:
                return None
            require(frame["demo"]["generation"] == OWNED.bounded_json(self.active)["generation"],
                    "Button belongs to stale generation")
        return frame, control

    def click(self, input_name, label=None):
        frame, control = wait_for("visible committed Button", lambda: self.control(input_name, label), 20)
        self.capture_sequence += 1
        (self.root / f"click-{self.capture_sequence:03d}.json").write_text(json.dumps(frame, indent=2))
        self.last_click_revision = frame["revision"]
        x = control["x"] + control["width"] // 2 + 1
        y = control["y"] + control["height"] // 2 + 1
        self.player.send(f"\x1b[<0;{x};{y}M\x1b[<0;{x};{y}m".encode())
        return frame

    def attempt(self):
        value = OWNED.bounded_json(self.active)
        require(value.get("playback_speed") in SPEEDS, "runner pacing evidence missing")
        require(isinstance(value.get("current_action"), str)
                and isinstance(value.get("action_count"), int) and value["action_count"] == 5,
                "runner action progress missing")
        require(value.get("phase"), "runner status missing")
        return value

    def lines(self):
        if not self.output.exists():
            return []
        require(self.output.stat().st_size <= 8192, "shell evidence exceeds budget")
        return self.output.read_text().splitlines()

    def shell_text(self):
        frame = self.terminal()
        return ''.join(row.rstrip() for row in frame["rows"]) if frame else ''

    def typed_prefix(self, index):
        # Remove only separators for observation, not to derive coordinates.
        # Embedded terminal soft wraps may include border/padding cells between
        # parts of one printable token. Button clicks always use raw cells.
        text = ''.join(character for character in self.shell_text()
                       if character.isascii() and (character.isalnum() or character == '_'))
        payload = self.payloads[index]
        start = text.find(payload[:32])
        if start < 0:
            return ''
        end = 0
        while end < len(payload) and start + end < len(text) and text[start + end] == payload[end]:
            end += 1
        return payload[:end]

    def partial(self, index):
        prefix = self.typed_prefix(index)
        return 32 <= len(prefix) < len(self.payloads[index]) and len(self.lines()) == index

    def paused(self):
        value = self.attempt()
        control = OWNED.bounded_json(self.active.parent / "control.json")
        return value if control.get("paused") and not value["cleanup_acknowledged"] else None

    def speed(self, speed):
        self.click("speed:" + speed)
        wait_for("generation-scoped pacing update", lambda: self.control_speed() == speed, 15)

    def control_speed(self):
        control = OWNED.bounded_json(self.active.parent / "control.json")
        require(control["generation"] == self.attempt()["generation"], "speed control generation mismatch")
        return control.get("playback_speed")

    def audience_capture(self, marker):
        if any(marker == payload[:32] for payload in self.payloads):
            # Fixed-canvas embedding clips centrally. Capture a real payload
            # segment visible in both retained projections, not a guessed row.
            terminal = self.terminal()
            snapshot = self.snapshot(self.audience_url)
            if terminal and snapshot:
                terminal_text = ''.join(terminal["rows"])
                audience_text = WEB.text(snapshot)
                payload = next(payload for payload in self.payloads if marker == payload[:32])
                shared = [payload[index:index + 16] for index in range(len(payload) - 15)
                          if payload[index:index + 16] in terminal_text
                          and payload[index:index + 16] in audience_text]
                if shared:
                    marker = shared[0]
        frame = self.frame(self.audience_url, marker)
        self.commit(self.audience, frame)
        require(not any(text in WEB.text(frame) for text in
                        ("[ Half ]", "[ Normal ]", "[ Double ]", "[ Instant ]", "[ Restart ]", "[ Pause ]", "[ Step ]")),
                "audience contains presenter controls")
        self.capture_sequence += 1
        stem = self.root / f"audience-{self.capture_sequence:03d}"
        stem.with_suffix(".json").write_text(json.dumps(frame, indent=2))
        require(WEB.rendered_status((self.audience.evaluate(WEB.PIXELS) or {}).get("status", ""), frame),
                "Canvas revision moved before capture")
        screenshot = self.audience.call("Page.captureScreenshot", {"format": "png"})
        require(WEB.rendered_status((self.audience.evaluate(WEB.PIXELS) or {}).get("status", ""), frame),
                "Canvas revision moved during capture")
        image = base64.b64decode(screenshot["data"], validate=True)
        require(0 < len(image) <= 8 * 1024 * 1024, "Canvas screenshot exceeds budget")
        stem.with_suffix(".png").write_bytes(image)
        terminal = wait_for("matching terminal cells", self.terminal, 15)
        require(marker in '\n'.join(terminal["rows"]), "terminal and audience do not share visible text")
        # Retain compositor availability/ownership without inventing metadata.
        stem.with_suffix(".terminal.json").write_text(json.dumps(terminal, indent=2))
        require(frame.get("unsupported_metadata") is not None, "compositor metadata availability absent")
        return frame

    def separate_host(self):
        host_root = self.root / "managed"
        host_root.mkdir(mode=0o700)
        for name in ("home", "config", "runtime", "data", "state", "logs", "cache", "tmp"):
            (host_root / name).mkdir(mode=0o700)
        config = host_root / "config/bmux.toml"
        config.write_text('[plugins]\nenabled = ["bmux.presenter", "bmux.playbook", '
                          '"bmux.sessions", "bmux.contexts", "bmux.clients", "bmux.tabs"]\n'
                          'search_paths = ' + json.dumps([str(self.args.plugins)]) + '\n')
        host_env = {key: value.replace(str(self.root), str(host_root))
                    for key, value in self.env.items()}
        # Artifacts and source/evidence paths do not move with host state.
        host_env["BMUX_MERMAID_WORKER_PATH"] = str(self.args.mermaid_worker)
        host_env["BMUX_PRESENTER_FRAME_EVIDENCE"] = str(self.evidence)
        command = [str(self.args.binary), "--runtime", "default", "--target", "local", "--config", str(config)]
        local_command = self.command
        try:
            self.command = command
            host = self.spawn(["server", "start", "--foreground-internal", "--no-pane-shell-integration"], env=host_env)
        finally:
            self.command = local_command
        self.own(host.child.pid)
        wait_for("separate owned managed provider", lambda: (host_root / "runtime/server.sock").is_socket()
                 and (host_root / "data/runtime/principal-id").is_file() and host.child.poll() is None, 20)
        require(host.child.pid != self.global_identity["pid"], "managed provider reused unrelated server")
        return command, host_env

    def start_player(self):
        web = ["--web", "--web-assets", self.args.assets, "--web-port", "0", "--web-auth", "none"]
        if not self.args.managed:
            return self.spawn(["present", "play", self.deck, "--allow-playbooks", "--presenter", *web], mirror=True)
        command, env = self.separate_host()
        local_command = self.command
        try:
            self.command = command
            return self.spawn(["present", "host", self.deck, "--managed", "--allow-playbooks",
                               "--presenter-credential", self.root / "presenter",
                               "--public-read", *web], mirror=True, env=env)
        finally:
            self.command = local_command

    def run(self):
        self.global_server = self.spawn(["server", "start", "--foreground-internal", "--no-pane-shell-integration"])
        self.global_identity = self.own(self.global_server.child.pid)
        principal = self.root / "data/runtime/principal-id"
        wait_for("unrelated disabled-provider server", lambda: (self.root / "runtime/server.sock").is_socket()
                 and principal.exists() and self.global_server.child.poll() is None, 20)
        require('disabled = ["bmux.playbook"]' in self.config.read_text(), "unrelated provider not explicitly disabled")
        self.principal_hash = WEB.digest(principal)
        check = self.spawn(["present", "check", self.deck])
        self.own(check.child.pid)
        require(check.child.wait(timeout=20) == 0, "deck check failed")
        self.stable("check created private execution", lambda: self.no_new_runtime({self.global_identity["pid"], check.child.pid}))
        self.player = self.start_player()
        self.own(self.player.child.pid)
        self.audience_url = self.url(self.player)
        self.audience = self.browser(self.audience_url)
        self.audience_capture(TITLE)
        self.player.send(b"\t")
        self.player.send(b"o")
        wait_for("opened committed modal", lambda: self.control("play-toggle", "Play"), 20)
        baseline = self.observed_bmux()
        self.stable("overlay created execution before Run", lambda: not self.attempts()
                    and not list(self.root.rglob("runtime.json")) and not self.service_roots()
                    and self.observed_bmux() <= baseline)
        self.click("speed:half")
        wait_for("prepared half speed committed", lambda: self.terminal() and
                 "speed: half" in '\n'.join(self.terminal()["rows"]).lower(), 15)
        self.click("play-toggle", "Play")
        self.active = wait_for("real first attempt", lambda: self.started(1), 25)
        first = self.active
        before, directory = self.attempt_record(first)
        frozen = self.frozen(first)
        require(directory != self.root / "runtime", "execution reused unrelated runtime")
        require(self.control_speed() == "half", "initial half pace lost")
        wait_for("half speed incrementally types shell command", lambda: self.partial(0), 25)
        half_prefix = len(self.typed_prefix(0))
        wait_for("half speed commits multiple incremental outputs", lambda:
                 half_prefix < len(self.typed_prefix(0)) < len(self.payloads[0]), 10)
        self.click("play-toggle", "Pause")
        wait_for("paused partial action", self.paused, 15)
        wait_for("Pause committed label", lambda: self.control("play-toggle", "Play"), 15)
        paused_prefix = self.typed_prefix(0)
        require(32 <= len(paused_prefix) < len(self.payloads[0]), "Pause missed partial command")
        progress = self.attempt()
        self.stable("Pause changed partial shell input", lambda: self.typed_prefix(0) == paused_prefix
                    and self.lines() == [] and self.attempt()["step"] == progress["step"], seconds=0.6)
        self.audience_capture(self.payloads[0][:32])
        self.speed("normal")
        self.click("play-toggle", "Play")
        wait_for("normal speed resumes without duplicate bytes", lambda: self.lines() == self.payloads[:1], 30)
        wait_for("normal speed incrementally types next command", lambda: self.partial(1), 25)
        require(self.attempt()["playback_speed"] == "normal", "runner normal pace not retained")
        normal_prefix = len(self.typed_prefix(1))
        wait_for("normal speed commits multiple incremental outputs", lambda:
                 normal_prefix < len(self.typed_prefix(1)) < len(self.payloads[1]), 10)
        generation = self.attempt()["generation"]
        closing_prefix = len(self.typed_prefix(1))
        self.click("close")
        wait_for("Close hides modal", lambda: self.terminal() and not self.terminal()["controls"], 15)
        # Hidden overlays intentionally publish no shell cells. Observe elapsed
        # progress only after reopening the same authoritative generation.
        OWNED.time.sleep(0.6)
        require(not self.attempt()["cleanup_acknowledged"], "Close stopped active attempt")
        self.player.send(b"o")
        wait_for("reopened active generation", lambda: self.control("play-toggle", "Pause"), 15)
        wait_for("hidden attempt continued real shell progress", lambda:
                 len(self.typed_prefix(1)) > closing_prefix, 10)
        require(self.attempt()["generation"] == generation, "reopening replaced generation")
        require(len(self.lines()) == 1, "Close progress observation missed second partial action")
        self.click("play-toggle", "Pause")
        wait_for("second paused partial action", self.paused, 15)
        wait_for("second Pause committed label", lambda: self.control("play-toggle", "Play"), 15)
        self.speed("double")
        self.speed("instant")
        require(self.lines() == self.payloads[:1], "speed change bypassed Pause")
        step_before = self.attempt()["step"]
        self.click("step")
        wait_for("Step finishes a whole action", lambda: self.lines() == self.payloads[:2], 20)
        wait_for("Step stays paused at next action", lambda: self.paused()
                 and self.attempt()["step"] == step_before + 1, 15)
        require(self.attempt()["step"] == step_before + 1, "Step advanced other than one action")
        self.stable("Step leaked characters into following action", lambda:
                    self.lines() == self.payloads[:2] and self.payloads[2][:32] not in self.shell_text(), seconds=0.6)
        self.click("play-toggle", "Play")
        wait_for("instant remaining command enters once", lambda: self.lines() == self.payloads, 20)
        wait_for("runner instant pace retained", lambda: self.attempt()["playback_speed"] == "instant", 15)
        self.audience_capture(self.payloads[2][:32])
        self.click("close")
        wait_for("navigation modal closed", lambda: self.terminal() and not self.terminal()["controls"], 15)
        self.player.send(b"\x1b[C")
        self.audience_capture("Retained final view")
        require(self.attempt()["generation"] == generation and not self.attempt()["cleanup_acknowledged"],
                "slide navigation destroyed shared work")
        self.player.send(b"\x1b[D")
        self.audience_capture(TITLE)
        self.player.send(b"\t")
        wait_for("returned demo focus", lambda: self.terminal() and TITLE in '\n'.join(self.terminal()["rows"]), 15)
        self.player.send(b"o")
        wait_for("shared work reopened after navigation", lambda: self.control("stop"), 15)
        require(self.attempt()["generation"] == generation, "navigation replaced shared generation")
        self.deck.write_text(self.original.replace(TITLE, RELOADED))
        self.audience_capture(RELOADED)
        require(self.attempts() == [first] and self.frozen(first) == frozen
                and self.attempt()["generation"] == generation
                and self.attempt()["session_id"] == before["session_id"], "compatible reload replaced approved attempt")
        self.source.write_text(self.approved + '# changed closure after approval\n')
        # Source closure is validated on deck reload, not by an independent
        # watcher that could silently replace frozen execution.
        self.deck.write_text(self.original.replace(TITLE, RELOADED) + '\n<!-- approval recheck -->\n')
        wait_for("changed approval rejected", lambda: b"Reload rejected" in self.player.bytes()
                 or b"changed after approval" in self.player.bytes()
                 or any(b"reload rejected" in process.bytes().lower() for process in self.processes), 20)
        require(self.frozen(first) == frozen and self.attempt()["generation"] == generation,
                "rejected reload mutated frozen execution")
        self.source.write_text(self.approved)
        self.click("stop")
        wait_for("Stop acknowledges actual cleanup", lambda: self.cleaned(first), 25)
        self.active = None
        wait_for("settled controls committed", lambda: self.terminal() and
                 "settled" in '\n'.join(self.terminal()["rows"]), 15)
        self.click("restart")
        self.active = wait_for("Restart creates fresh attempt", lambda: self.started(2), 25)
        require(self.attempt()["generation"] != generation, "Restart reused generation")
        require(self.attempt()["session_id"] != before["session_id"], "Restart reused session")
        self.unrelated_alive()
        require(not self.args.managed or not self.service_roots(), "managed host created lazy local service")
        self.passed("committed Button clicks, four paces, whole-action Step and no duplicate shell Enter")
        self.passed("Close/navigation retain generation; approval/reload retain frozen execution")
        self.passed("read-only audience Canvas hides controls and displays real shell")
        if self.args.managed:
            # The host owns shared execution; detaching its viewer is not a
            # destruction request. Explicitly stop through the resource authority.
            wait_for("restart controls committed", lambda: self.control("stop"), 15)
            self.click("stop")
            wait_for("managed explicit Stop cleanup", lambda: self.cleaned(self.active), 25)
        self.player.send(b"q")
        wait_for("q exits presenter viewer", lambda: self.player.child.poll() is not None, 25)
        require(self.player.child.returncode == 0, "presenter q failed")
        wait_for("all attempt runtimes removed", lambda: all(self.cleaned(path) for path in self.attempts()), 25)
        self.unrelated_alive()
        if not self.args.managed:
            self.scan()
            private_services = {path: record for path, record in self.service_records().items()
                                if Path(path) != self.root / "runtime"}
            wait_for("q removes private services and sockets", lambda: all(
                not OWNED.alive(record) and not (Path(path) / "server.pid").exists()
                and not list(Path(path).glob("*.sock"))
                for path, record in private_services.items()), 20)
        self.attempt_evidence = [{"path": str(path.relative_to(self.root)),
                                  "generation": OWNED.bounded_json(path)["generation"],
                                  "cleanup_acknowledged": True} for path in self.attempts()]
        self.passed("Stop/Restart/q clean owned attempts; unrelated disabled-provider identity survives")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("binary", "plugins", "assets", "browser", "mermaid-worker"):
        parser.add_argument("--" + name, type=lambda value: Path(value).resolve(strict=True), required=True)
    parser.add_argument("--state-parent", type=lambda value: Path(value).resolve(strict=True), default=Path("/tmp"))
    parser.add_argument("--managed", action="store_true", help="distinct owned provider-enabled managed host")
    args = parser.parse_args()
    require(Path("/proc/self/stat").is_file(), "Linux process identity required")
    for name in ("binary", "browser"):
        require(getattr(args, name).is_file() and os.access(getattr(args, name), os.X_OK), "explicit executable required")
    require(args.plugins.is_dir() and args.assets.is_dir(), "explicit staged artifacts required")
    for name in WEB.ASSETS:
        require((args.assets / name).is_file(), "missing built browser asset")
    suite = Acceptance(args)
    try:
        suite.run()
    except BaseException as error:
        # Foreign errors can contain capabilities; private failure diagnostics
        # record only exception type and our fixed assertion labels.
        suite.failed = type(error).__name__
        for process in suite.processes:
            if hasattr(process, "bytes"):
                (suite.root / f"process-{process.child.pid}.private.log").write_bytes(process.bytes())
        label = str(error) if isinstance(error, AssertionError) else type(error).__name__
        import traceback
        locations = [{"file": Path(entry.filename).name, "line": entry.lineno,
                      "function": entry.name} for entry in traceback.extract_tb(error.__traceback__)]
        (suite.root / "failure.json").write_text(json.dumps({"type": type(error).__name__, "assertion": label,
                                                           "locations": locations}))
    return 0 if suite.cleanup() else 1


if __name__ == "__main__":
    try:
        sys.exit(main())
    except Exception as error:
        print(json.dumps({"status": "FAIL", "failure": type(error).__name__}), file=sys.stderr)
        sys.exit(1)
