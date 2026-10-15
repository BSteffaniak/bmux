#!/usr/bin/env python3
"""Agent-paced Flappy Bird. Standalone application; BMUX is only its terminal host.

Uses the Jev judgement wire contract also used by Bcode, without its runtime.
Only Python's standard library is required. No credentials or upstream bodies
are printed. One disposable HTTP worker bounds cancellation and request lifetime.
"""
from __future__ import annotations

import argparse
import curses
import dataclasses
import http.client
import json
import math
import multiprocessing
import os
from pathlib import Path
import random
import signal
import socket
import stat
import sys
import time
from urllib.parse import urlsplit

MODEL = "jev-1.13.0"
MAX_RESPONSE = 64 * 1024
REQUEST_TIMEOUT = 15.0
TICK_SECONDS = 1 / 30
DECISION_TICKS = 5


class DemoError(Exception):
    """Only fixed, safe messages may cross the display boundary."""


@dataclasses.dataclass(frozen=True)
class Config:
    backend: str
    base_url: str
    model: str
    key: str = dataclasses.field(repr=False)

    @classmethod
    def environment(cls):
        backend = os.environ.get("JEV_BACKEND", "hosted")
        if backend not in ("hosted", "official"):
            raise DemoError("JEV_BACKEND must be hosted or official")
        official = backend == "official"
        key = os.environ.get("TYPESAFE_API_KEY" if official else "JEV_API_KEY", "")
        if not key or any(ord(c) < 33 or ord(c) > 126 for c in key):
            raise DemoError("Set the API key for the selected backend")
        base = os.environ.get("JEV_BASE_URL", "https://api.typesafe.ai" if official
                              else "https://jevtypesafeai.com").rstrip("/")
        validate_endpoint(base, backend)
        model = os.environ.get("JEV_MODEL", MODEL)
        if not model or len(model) > 128 or not all(c.isalnum() or c in "-._" for c in model):
            raise DemoError("Invalid JEV_MODEL")
        return cls(backend, base, model, key)


def validate_endpoint(base, backend):
    try:
        url = urlsplit(base)
        port = url.port
    except ValueError:
        raise DemoError("Invalid Jev endpoint") from None
    del port
    if (not url.hostname or url.username is not None or url.password is not None
            or url.query or url.fragment or url.path not in ("", "/")
            or (url.scheme != "https" and not
                (url.scheme == "http" and url.hostname == "127.0.0.1"))):
        raise DemoError("Jev endpoint must be HTTPS (HTTP only on 127.0.0.1)")
    if ((url.hostname == "api.typesafe.ai" and backend != "official")
            or (url.hostname == "jevtypesafeai.com" and backend != "hosted")):
        raise DemoError("Jev endpoint does not match selected backend")


@dataclasses.dataclass
class Pipe:
    x: float
    gap_y: float
    scored: bool = False


class Game:
    width = 100
    height = 30
    bird_x = 20
    gravity = 0.06
    flap_velocity = -0.65
    pipe_speed = 0.45
    pipe_width = 5
    gap_height = 11
    spacing = 34

    def __init__(self, seed=7):
        self.random = random.Random(seed)
        self.attempt = 0
        self.best = 0
        self.reset()

    def reset(self):
        self.attempt += 1
        self.y = self.height / 2
        self.velocity = 0.0
        self.score = 0
        self.dead = False
        self.ticks = 0
        self.pipes = [Pipe(x, self.random.uniform(10, 20)) for x in (56, 90, 124)]

    def tick(self, flap=False):
        if self.dead:
            return
        if flap:
            self.velocity = self.flap_velocity
        self.velocity += self.gravity
        self.y += self.velocity
        self.ticks += 1
        for pipe in self.pipes:
            pipe.x -= self.pipe_speed
            if not pipe.scored and pipe.x + self.pipe_width < self.bird_x - 1:
                pipe.scored = True
                self.score += 1
                self.best = max(self.best, self.score)
            if (pipe.x - 1 <= self.bird_x <= pipe.x + self.pipe_width + 1
                    and abs(self.y - pipe.gap_y) + 0.7 >= self.gap_height / 2):
                self.dead = True
        if self.y < 0.7 or self.y >= self.height - 0.7:
            self.dead = True
        self.pipes = [p for p in self.pipes if p.x + self.pipe_width >= 0]
        while len(self.pipes) < 3:
            self.pipes.append(Pipe(self.pipes[-1].x + self.spacing,
                                   self.random.uniform(10, 20)))

    def observation(self):
        return {
            "coordinates": "x rightwards, y downwards; units per simulation tick",
            "world": {"width": self.width, "height": self.height},
            "bird": {"x": self.bird_x, "y": round(self.y, 3),
                     "velocity_y": round(self.velocity, 3), "radius": 0.7},
            "physics": {"gravity": self.gravity, "flap_velocity": self.flap_velocity,
                        "pipe_speed": self.pipe_speed, "pipe_width": self.pipe_width,
                        "gap_height": self.gap_height},
            "next_pipes": [{"x": round(p.x, 3), "gap_y": round(p.gap_y, 3)}
                           for p in self.pipes if p.x + self.pipe_width >= self.bird_x - 1],
            "advance_ticks": DECISION_TICKS,
            "score": self.score,
        }


def request_payload(config, observation):
    return {"model": config.model, "state": observation, "questions": {
        "flap": {"type": "noul", "instructions": (
            "Should the bird flap NOW to survive and pass the next pipe? "
            "Return the probability that flapping is better than waiting. "
            "The game advances exactly advance_ticks after this decision. "
            "Flap sets vertical velocity, it does not add to it. Predict gravity, "
            "avoid floor and ceiling, and align with the upcoming pipe gap. "
            "No other pilot or safety controller will correct your action.")}}}


def decode_answer(body, model):
    try:
        wire = json.loads(body)
        if wire["model"] != model or set(wire["answers"]) != {"flap"}:
            raise ValueError
        answer = wire["answers"]["flap"]
        probability = answer["noul"]
        if (answer["type"] != "noul" or type(probability) not in (float, int)
                or not math.isfinite(probability) or not 0 <= probability <= 1):
            raise ValueError
        return float(probability)
    except (ValueError, TypeError, KeyError, RecursionError):
        raise DemoError("Invalid Jev response or model mismatch") from None


def judge(config, observation):
    """No redirects, proxy environment, retries, upstream error text, or unbounded body."""
    validate_endpoint(config.base_url, config.backend)
    url = urlsplit(config.base_url)
    connection_type = http.client.HTTPSConnection if url.scheme == "https" else http.client.HTTPConnection
    connection = connection_type(url.hostname, url.port, timeout=REQUEST_TIMEOUT)
    path = "/v1/systemone" if config.backend == "official" else "/api/v1/decide"
    try:
        connection.request("POST", path, json.dumps(request_payload(config, observation)),
                           {"Authorization": "Bearer " + config.key,
                            "Content-Type": "application/json"})
        response = connection.getresponse()
        if response.status != 200:
            errors = {401: "Credential rejected", 403: "Credential rejected",
                      402: "Quota exhausted", 429: "Rate limited"}
            raise DemoError(errors.get(response.status, "Jev service unavailable"))
        body = response.read(MAX_RESPONSE + 1)
        if len(body) > MAX_RESPONSE:
            raise DemoError("Jev response exceeds size limit")
        return decode_answer(body, config.model)
    except (OSError, http.client.HTTPException):
        raise DemoError("Jev transport unavailable") from None
    finally:
        connection.close()


def request_worker(sender, config, observation):
    try:
        result = {"probability": judge(config, observation)}
    except DemoError as error:
        result = {"error": str(error)}
    except Exception:
        result = {"error": "Jev request failed"}
    try:
        sender.send_bytes(json.dumps(result).encode())
    finally:
        sender.close()


class Request:
    """One in-flight process. Termination reaps it before another can be admitted."""
    def __init__(self, config, observation):
        ctx = multiprocessing.get_context("spawn")
        self.receiver, sender = ctx.Pipe(duplex=False)
        self.process = ctx.Process(target=request_worker, args=(sender, config, observation), daemon=True)
        self.started = time.monotonic()
        try:
            self.process.start()
        except BaseException:
            self.receiver.close()
            sender.close()
            raise
        sender.close()

    def poll(self):
        if time.monotonic() - self.started >= REQUEST_TIMEOUT:
            self.close()
            return {"error": "Jev request timed out"}
        if self.receiver.poll():
            try:
                result = json.loads(self.receiver.recv_bytes(MAX_RESPONSE))
            except (EOFError, OSError, ValueError):
                result = {"error": "Jev request failed"}
            self.close()
            return result
        if not self.process.is_alive():
            self.close()
            return {"error": "Jev worker exited"}
        return None

    def close(self):
        if self.process.is_alive():
            self.process.terminate()
        self.process.join(timeout=0.2)
        if self.process.is_alive():
            self.process.kill()
            self.process.join(timeout=0.2)
        self.receiver.close()
        self.process.close()


class Controller:
    def __init__(self, config, seed=7, request_factory=Request):
        self.game = Game(seed)
        self.config = config
        self.request_factory = request_factory
        self.request = None
        self.paused = False
        self.stopped = False
        self.remaining = 0
        self.flap = False
        self.probability = None
        self.latency_ms = 0
        self.requests = 0
        self.failures = 0
        self.retry_at = 0.0
        self.next_tick = 0.0
        self.message = "Ready - Jev agent-paced gameplay"

    def cancel(self):
        if self.request is not None:
            self.request.close()
            self.request = None

    def command(self, command):
        if command == "pause":
            self.paused = True
            self.cancel()
        elif command == "resume":
            self.paused = False
        elif command == "restart":
            self.cancel()
            self.game.reset()
            self.remaining = 0
            self.retry_at = 0
            self.failures = 0
        elif command == "stop":
            self.stopped = True
            self.cancel()
        elif command != "status":
            raise DemoError("Unknown control")
        return self.status()

    def status(self):
        return {"paused": self.paused, "stopped": self.stopped,
                "attempt": self.game.attempt, "score": self.game.score,
                "best": self.game.best, "requests": self.requests,
                "in_flight": self.request is not None, "message": self.message}

    def update(self, now):
        if self.paused or self.stopped:
            return
        if self.game.dead:
            if now >= self.retry_at:
                self.game.reset()
            return
        if self.remaining:
            if now >= self.next_tick:
                self.game.tick(self.flap)
                self.flap = False
                self.remaining -= 1
                self.next_tick = now + TICK_SECONDS
                if self.game.dead:
                    self.remaining = 0
                    self.retry_at = now + 1.5
                    self.message = "Crashed! Next attempt shortly..."
            return
        if self.request is not None:
            result = self.request.poll()
            if result is None:
                return
            self.latency_ms = round((now - self.request.started) * 1000)
            self.request = None
            if "error" in result:
                self.failures = min(self.failures + 1, 6)
                self.retry_at = now + min(30, 2 ** self.failures)
                self.message = result["error"] + " - paused physics; retrying"
                return
            self.failures = 0
            self.probability = result["probability"]
            self.flap = self.probability >= 0.5
            self.remaining = DECISION_TICKS
            self.next_tick = now
            self.message = "FLAP" if self.flap else "WAIT"
        elif now >= self.retry_at:
            self.request = self.request_factory(self.config, self.game.observation())
            self.requests += 1
            self.message = "Jev deciding... (physics waiting)"


COMMANDS = ("pause", "resume", "restart", "stop", "status")


class Controls:
    """Optional private application control channel; not a BMUX service contract."""
    def __init__(self, path):
        self.path = Path(path)
        parent = self.path.parent
        info = parent.stat()
        if (not self.path.is_absolute() or info.st_uid != os.getuid()
                or stat.S_IMODE(info.st_mode) & 0o077):
            raise DemoError("Control socket requires an owned private directory (0700)")
        self.socket = socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM)
        try:
            # A forcibly stopped terminal can leave a socket inode. Reclaim only
            # an owned socket in this private directory whose endpoint is gone;
            # never replace an active listener or a regular file/symlink.
            try:
                previous = self.path.lstat()
            except FileNotFoundError:
                previous = None
            if previous is not None:
                if not stat.S_ISSOCK(previous.st_mode) or previous.st_uid != os.getuid():
                    raise DemoError("Control path is not an owned socket")
                with socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM) as probe:
                    try:
                        probe.connect(str(self.path))
                    except ConnectionRefusedError:
                        if self.path.lstat().st_ino != previous.st_ino:
                            raise DemoError("Control socket ownership changed") from None
                        self.path.unlink()
                    else:
                        raise DemoError("Control socket already has a live application")
            self.socket.bind(str(self.path))
            os.chmod(self.path, 0o600)
            self.socket.setblocking(False)
            self.identity = self.path.stat().st_ino
        except BaseException:
            self.socket.close()
            raise

    def poll(self, controller):
        # Bounded work per frame, including under a misbehaving local sender.
        for _ in range(8):
            try:
                data, address = self.socket.recvfrom(128)
            except BlockingIOError:
                break
            try:
                command = data.decode("ascii")
                result = controller.command(command)
            except (UnicodeError, DemoError):
                result = {"error": "Unknown control"}
            if address:
                try:
                    self.socket.sendto(json.dumps(result).encode(), address)
                except OSError:
                    pass

    def close(self):
        self.socket.close()
        try:
            if self.path.lstat().st_ino == self.identity:
                self.path.unlink()
        except FileNotFoundError:
            pass


def send_control(path, command):
    # Bind the reply address in the same protected directory; no public socket.
    import tempfile
    with tempfile.TemporaryDirectory(prefix="reply-", dir=Path(path).parent) as directory:
        with socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM) as client:
            client.settimeout(2)
            client.bind(str(Path(directory) / "s"))
            client.sendto(command.encode(), path)
            return json.loads(client.recv(4096))


def draw(screen, controller):
    rows, cols = screen.getmaxyx()
    screen.erase()

    def text(y, x, value, color=0):
        if 0 <= y < rows - 1 and 0 <= x < cols - 1:
            screen.addnstr(y, x, value, cols - x - 1, color)

    game = controller.game
    cyan = curses.color_pair(1)
    green = curses.color_pair(2)
    yellow = curses.color_pair(3) | curses.A_BOLD
    text(0, 1, "JEV FLAPPY BIRD | agent-paced | LIVE", cyan | curses.A_BOLD)
    text(1, 1, f"Score {game.score}   Best {game.best}   Attempt {game.attempt}   Requests {controller.requests}")
    status = "PAUSED" if controller.paused else controller.message
    text(2, 1, status, yellow)
    probability = "--" if controller.probability is None else f"{controller.probability:.2f}"
    text(3, 1, f"P(flap) {probability}   Last latency {controller.latency_ms} ms   {controller.config.model}")
    text(rows - 2, 1, "p pause/resume | r restart | q quit | private CLI controls available", cyan)
    if rows < 15 or cols < 45:
        text(6, 1, "Resize terminal to at least 45 x 15")
        return
    field_height = rows - 8
    field_width = cols - 3
    for pipe in game.pipes:
        left = max(0, math.floor(pipe.x / game.width * field_width))
        right = min(field_width, math.ceil((pipe.x + game.pipe_width) / game.width * field_width))
        for y in range(field_height):
            world_y = (y + 0.5) / field_height * game.height
            if abs(world_y - pipe.gap_y) >= game.gap_height / 2:
                text(y + 5, left + 1, "#" * max(0, right - left), green)
    x = round(game.bird_x / game.width * field_width) + 1
    y = min(field_height - 1, max(0, int(game.y / game.height * field_height))) + 5
    text(y, x, ">o", yellow)
    text(rows - 3, 1, "=" * field_width, green)


def run(screen, controller, controls):
    curses.curs_set(0)
    screen.nodelay(True)
    if curses.has_colors():
        curses.start_color()
        curses.use_default_colors()
        for index, color in ((1, curses.COLOR_CYAN), (2, curses.COLOR_GREEN), (3, curses.COLOR_YELLOW)):
            curses.init_pair(index, color, -1)
    while not controller.stopped:
        key = screen.getch()
        if key in (ord("q"), 27):
            controller.command("stop")
        elif key == ord("p"):
            controller.command("resume" if controller.paused else "pause")
        elif key == ord("r"):
            controller.command("restart")
        if controls:
            controls.poll(controller)
        controller.update(time.monotonic())
        draw(screen, controller)
        screen.refresh()
        time.sleep(1 / 30)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--seed", type=int, default=7)
    parser.add_argument("--control", choices=COMMANDS)
    args = parser.parse_args()
    path = os.environ.get("JEV_FLAPPY_SOCKET")
    try:
        if args.control:
            if not path:
                raise DemoError("Set JEV_FLAPPY_SOCKET for application controls")
            print(json.dumps(send_control(path, args.control)))
            return 0
        config = Config.environment()
        controls = Controls(path) if path else None
        controller = Controller(config, args.seed)
        previous = {}
        try:
            # The application owns only its worker and its optional control socket.
            # BMUX owns the terminal process and tears down its managed sandbox.
            for number in (signal.SIGTERM, signal.SIGHUP, signal.SIGINT):
                previous[number] = signal.signal(number, lambda *_: setattr(controller, "stopped", True))
            curses.wrapper(run, controller, controls)
        finally:
            controller.cancel()
            if controls:
                controls.close()
            for number, handler in previous.items():
                signal.signal(number, handler)
        return 0
    except DemoError as error:
        print(str(error), file=sys.stderr)
    except (OSError, curses.error):
        print("Demo unavailable: check terminal, private control directory, and configuration", file=sys.stderr)
    return 1


if __name__ == "__main__":
    sys.exit(main())
