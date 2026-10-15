"""Offline physics, protocol, lifecycle, and private-control tests; no provider spend."""
import dataclasses
import json
import os
from pathlib import Path
import socket
import tempfile
import threading
import time
import unittest
from unittest.mock import patch
from http.server import BaseHTTPRequestHandler, HTTPServer

import jev_flappy as demo


CONFIG = demo.Config("hosted", "https://jevtypesafeai.com", demo.MODEL, "test-only-key")


class FakeRequest:
    instances = []
    result = {"probability": 0.9}

    def __init__(self, config, observation):
        self.started = 0
        self.closed = False
        self.instances.append(self)

    def poll(self):
        if self.result is not None:
            self.closed = True
        return self.result

    def close(self):
        self.closed = True


class GameTests(unittest.TestCase):
    def test_deterministic_and_bounded_across_many_attempts(self):
        a, b = demo.Game(7), demo.Game(7)
        for _ in range(5000):
            if a.dead:
                a.reset()
                b.reset()
            a.tick(a.velocity > 0.3)
            b.tick(b.velocity > 0.3)
            self.assertEqual(a.observation(), b.observation())
            self.assertEqual(len(a.pipes), 3)
        self.assertGreater(a.attempt, 1)

    def test_collision_and_score(self):
        game = demo.Game()
        game.y = 0.8
        game.tick(True)
        self.assertTrue(game.dead)
        game.reset()
        game.pipes[0].x = game.bird_x - game.pipe_width - 2
        game.tick()
        self.assertEqual(game.score, 1)
        self.assertEqual(game.best, 1)
        game.reset()
        self.assertEqual(game.score, 0)
        self.assertEqual(game.best, 1)
        game.pipes[0].x = game.bird_x
        game.y = 3
        game.tick()
        self.assertTrue(game.dead)


class ProtocolTests(unittest.TestCase):
    def test_configuration_is_explicit_and_secret_is_not_repr(self):
        with patch.dict(os.environ, {"JEV_API_KEY": "private-key"}, clear=True):
            config = demo.Config.environment()
            self.assertEqual(config.backend, "hosted")
            self.assertNotIn("private-key", repr(config))
        with patch.dict(os.environ, {"TYPESAFE_API_KEY": "official-key", "JEV_BACKEND": "official"}, clear=True):
            self.assertEqual(demo.Config.environment().base_url, "https://api.typesafe.ai")
        with patch.dict(os.environ, {}, clear=True), self.assertRaises(demo.DemoError):
            demo.Config.environment()

    def test_unsafe_endpoints_rejected(self):
        for url in ("http://example.com", "https://user:secret@example.com",
                    "https://example.com?key=x", "https://example.com/#x",
                    "https://example.com/custom", "https://api.typesafe.ai",
                    "file:///tmp/a", "https://example.com:invalid"):
            with self.subTest(url=url), self.assertRaises(demo.DemoError):
                demo.validate_endpoint(url, "hosted")
        demo.validate_endpoint("http://127.0.0.1:1234", "hosted")

    def test_response_validation(self):
        def body(probability=0.9, **kwargs):
            return json.dumps({"model": demo.MODEL, "answers": {
                "flap": {"type": "noul", "noul": probability}}, **kwargs})
        self.assertEqual(demo.decode_answer(body(), demo.MODEL), 0.9)
        for invalid in (body(True), body(-1), body(2), body(float("nan")),
                        body(model="wrong"), body(answers={}), '[]', 'not json'):
            with self.subTest(invalid=invalid), self.assertRaises(demo.DemoError):
                demo.decode_answer(invalid, demo.MODEL)

    def test_loopback_wire_and_error_redaction(self):
        observed = []

        class Handler(BaseHTTPRequestHandler):
            status = 200
            body = json.dumps({"model": demo.MODEL, "answers": {
                "flap": {"type": "noul", "noul": 0.7}}}).encode()

            def do_POST(self):
                observed.append((self.path, self.headers["Authorization"],
                                 json.loads(self.rfile.read(int(self.headers["Content-Length"])))))
                self.send_response(self.status)
                self.send_header("Content-Length", str(len(self.body)))
                self.end_headers()
                self.wfile.write(self.body)

            def log_message(self, *_):
                pass

        with HTTPServer(("127.0.0.1", 0), Handler) as server:
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            try:
                config = dataclasses.replace(CONFIG, base_url=f"http://127.0.0.1:{server.server_port}")
                self.assertEqual(demo.judge(config, demo.Game().observation()), 0.7)
                self.assertEqual(observed[-1][0], "/api/v1/decide")
                self.assertEqual(observed[-1][1], "Bearer test-only-key")
                self.assertEqual(observed[-1][2]["questions"]["flap"]["type"], "noul")
                self.assertEqual(demo.judge(dataclasses.replace(config, backend="official"), {}), 0.7)
                self.assertEqual(observed[-1][0], "/v1/systemone")
                # Exercise the real spawned transport, not only the direct client.
                request = demo.Request(config, demo.Game().observation())
                try:
                    deadline = time.monotonic() + 5
                    result = None
                    while result is None and time.monotonic() < deadline:
                        result = request.poll()
                        time.sleep(0.01)
                    self.assertEqual(result, {"probability": 0.7})
                finally:
                    if result is None:
                        request.close()
                for status in (302, 401, 402, 429, 500):
                    Handler.status = status
                    Handler.body = b"upstream-secret"
                    with self.assertRaises(demo.DemoError) as error:
                        demo.judge(config, {})
                    self.assertNotIn("upstream-secret", str(error.exception))
                Handler.status = 200
                Handler.body = b"x" * (demo.MAX_RESPONSE + 1)
                with self.assertRaisesRegex(demo.DemoError, "size limit"):
                    demo.judge(config, {})
            finally:
                server.shutdown()
                thread.join(2)


class LifecycleTests(unittest.TestCase):
    def setUp(self):
        FakeRequest.instances = []
        FakeRequest.result = {"probability": 0.9}
        self.controller = demo.Controller(CONFIG, request_factory=FakeRequest)

    def test_waits_for_decision_and_no_overlapping_requests(self):
        c = self.controller
        FakeRequest.result = None
        initial = c.game.observation()
        for i in range(100):
            c.update(i / 30)
        self.assertEqual(c.game.observation(), initial)
        self.assertEqual(len(FakeRequest.instances), 1)
        FakeRequest.result = {"probability": 0.9}
        c.update(4)
        self.assertEqual(c.remaining, demo.DECISION_TICKS)
        c.update(5)
        self.assertLess(c.game.velocity, 0)
        self.assertEqual(c.game.ticks, 1)

    def test_pause_cancels_and_restart_discards_stale_result(self):
        c = self.controller
        c.update(0)
        c.command("pause")
        self.assertTrue(FakeRequest.instances[0].closed)
        self.assertIsNone(c.request)
        c.update(100)
        self.assertEqual(c.requests, 1)
        c.command("resume")
        c.update(101)
        c.command("restart")
        self.assertTrue(FakeRequest.instances[-1].closed)
        self.assertEqual(c.game.attempt, 2)
        self.assertEqual(c.remaining, 0)
        c.command("stop")
        c.update(200)
        self.assertEqual(c.requests, 2)

    def test_retry_backoff_and_no_fake_pilot(self):
        c = self.controller
        FakeRequest.result = {"error": "Rate limited"}
        c.update(0)
        c.update(1)
        self.assertEqual(c.game.ticks, 0)
        self.assertIsNone(c.request)
        c.update(2)
        self.assertEqual(c.requests, 1)
        c.update(4)
        self.assertEqual(c.requests, 2)

    def test_auto_restart_and_unlimited_lifetime(self):
        c = self.controller
        c.game.dead = True
        c.retry_at = 2
        c.update(1)
        self.assertEqual(c.game.attempt, 1)
        c.update(3)
        self.assertEqual(c.game.attempt, 2)
        c.update(10**9)
        self.assertFalse(c.stopped)
        self.assertEqual(c.requests, 1)

    def test_stale_socket_reclaimed_but_live_socket_and_files_preserved(self):
        with tempfile.TemporaryDirectory(prefix="jf-") as directory:
            path = Path(directory) / "s"
            orphan = socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM)
            orphan.bind(str(path))
            orphan.close()
            controls = demo.Controls(str(path))
            try:
                with self.assertRaises(demo.DemoError):
                    demo.Controls(str(path))
                self.assertTrue(path.exists())
            finally:
                controls.close()
            path.write_text("do not replace")
            with self.assertRaises(demo.DemoError):
                demo.Controls(str(path))
            self.assertEqual(path.read_text(), "do not replace")

    def test_private_socket_controls_and_cleanup(self):
        with tempfile.TemporaryDirectory(prefix="jf-") as directory:
            path = str(Path(directory) / "s")
            controls = demo.Controls(path)
            try:
                with socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM) as client:
                    client.sendto(b"pause", path)
                    controls.poll(self.controller)
                    self.assertTrue(self.controller.paused)
                    client.sendto(b"restart", path)
                    controls.poll(self.controller)
                    self.assertEqual(self.controller.game.attempt, 2)
                    client.sendto(b"stop", path)
                    controls.poll(self.controller)
                    self.assertTrue(self.controller.stopped)
                self.assertEqual(Path(path).stat().st_mode & 0o777, 0o600)
            finally:
                controls.close()
            self.assertFalse(Path(path).exists())
        with self.assertRaises(demo.DemoError):
            demo.Controls("relative.sock")


if __name__ == "__main__":
    unittest.main()
