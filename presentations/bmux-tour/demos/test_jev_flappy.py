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


def answer(action="FLAP", confidence=0.9):
    return {"action": action, "confidence": confidence,
            "probabilities": {"FLAP": 0.7, "WAIT": 0.3}}


def wire_answer(action="FLAP"):
    return {"type": "choice", "choice": action, "confidence": 0.9,
            "probabilities": {"FLAP": 0.7, "WAIT": 0.3}}


class FakeRequest:
    instances = []
    result = answer()

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

    def test_forecasts_are_exact_and_do_not_change_live_state(self):
        import copy
        for y, velocity in [(15, 0), (1.5, -0.5), (28.5, 1.0), (12, 0.3)]:
            game = demo.Game()
            game.y, game.velocity = y, velocity
            before = copy.deepcopy(game.__dict__)
            random_state = game.random.getstate()
            for flap in (False, True):
                forecast = game.forecast(flap)
                projected = copy.deepcopy(game)
                expected = []
                collision = None
                for tick in range(1, 16):
                    projected.tick(flap and tick == 1)
                    if tick % demo.DECISION_TICKS == 0 or projected.dead:
                        expected.append({"tick": tick, "y": round(projected.y, 3),
                                         "velocity_y": round(projected.velocity, 3)})
                    if projected.dead:
                        collision = tick
                        break
                self.assertEqual(forecast["samples"], expected)
                self.assertEqual(forecast["collision_tick_if_coasting"], collision)
            self.assertEqual(game.y, before["y"])
            self.assertEqual(game.velocity, before["velocity"])
            self.assertEqual(game.pipes, before["pipes"])
            self.assertEqual(game.random.getstate(), random_state)

    def test_ceiling_requires_wait_and_floor_requires_flap(self):
        game = demo.Game()
        game.y, game.velocity = 1.5, 0
        observation = game.observation()
        self.assertFalse(observation["action_forecasts"]["FLAP"]["safe_through_next_decision"])
        self.assertTrue(observation["action_forecasts"]["WAIT"]["safe_through_next_decision"])
        game.y, game.velocity = 28, 0.6
        observation = game.observation()
        self.assertTrue(observation["action_forecasts"]["FLAP"]["safe_through_next_decision"])
        self.assertFalse(observation["action_forecasts"]["WAIT"]["safe_through_next_decision"])

    def test_observations_support_multiple_pipes_with_test_only_choice_policy(self):
        # This is an offline observability/physics check, NOT a production pilot
        # or evidence that the remote model will choose the same actions.
        for seed in range(10):
            game = demo.Game(seed)
            for _ in range(200):
                observation = game.observation()
                def cost(action):
                    forecast = observation["action_forecasts"][action]
                    return (not forecast["safe_through_next_decision"],
                            abs(forecast["samples"][0]["y"] - observation["target"]["center_y"]))
                action = min(("WAIT", "FLAP"), key=cost)
                for tick in range(demo.DECISION_TICKS):
                    game.tick(action == "FLAP" and tick == 0)
                self.assertFalse(game.dead)
            self.assertGreaterEqual(game.score, 10)

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
        def body(response=None, **kwargs):
            return json.dumps({"model": demo.MODEL, "answers": {
                "action": wire_answer() if response is None else response}, **kwargs})
        self.assertEqual(demo.decode_answer(body(), demo.MODEL), answer())
        # Explicit WAIT wins even with tied probabilities/high confidence.
        response = wire_answer("WAIT")
        response["probabilities"] = {"FLAP": 0.5, "WAIT": 0.5}
        self.assertEqual(demo.decode_answer(body(response), demo.MODEL)["action"], "WAIT")
        for response in ({**wire_answer(), "choice": "unknown"},
                         {**wire_answer(), "type": "noul"},
                         {**wire_answer(), "confidence": True},
                         {**wire_answer(), "probabilities": {"FLAP": float("nan"), "WAIT": 0}},
                         {**wire_answer(), "probabilities": {"FLAP": 0.7}},
                         {**wire_answer(), "probabilities": {"FLAP": 0.7, "WAIT": 0.9}}):
            with self.subTest(response=response), self.assertRaises(demo.DemoError):
                demo.decode_answer(body(response), demo.MODEL)
        for invalid in (body(model="wrong"), body(answers={}), '[]', 'not json'):
            with self.subTest(invalid=invalid), self.assertRaises(demo.DemoError):
                demo.decode_answer(invalid, demo.MODEL)

    def test_loopback_wire_and_error_redaction(self):
        observed = []

        class Handler(BaseHTTPRequestHandler):
            status = 200
            body = json.dumps({"model": demo.MODEL, "answers": {
                "action": wire_answer()}}).encode()

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
                self.assertEqual(demo.judge(config, demo.Game().observation()), answer())
                self.assertEqual(observed[-1][0], "/api/v1/decide")
                self.assertEqual(observed[-1][1], "Bearer test-only-key")
                self.assertEqual(observed[-1][2]["questions"]["action"]["type"], "choice")
                self.assertEqual(set(observed[-1][2]["questions"]["action"]["criteria"]), {"FLAP", "WAIT"})
                self.assertEqual(demo.judge(dataclasses.replace(config, backend="official"), {}), answer())
                self.assertEqual(observed[-1][0], "/v1/systemone")
                # Exercise the real spawned transport, not only the direct client.
                request = demo.Request(config, demo.Game().observation())
                try:
                    deadline = time.monotonic() + 5
                    result = None
                    while result is None and time.monotonic() < deadline:
                        result = request.poll()
                        time.sleep(0.01)
                    self.assertEqual(result, answer())
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
        FakeRequest.result = answer()
        self.controller = demo.Controller(CONFIG, request_factory=FakeRequest)

    def test_waits_for_decision_and_no_overlapping_requests(self):
        c = self.controller
        FakeRequest.result = None
        initial = c.game.observation()
        for i in range(100):
            c.update(i / 30)
        self.assertEqual(c.game.observation(), initial)
        self.assertEqual(len(FakeRequest.instances), 1)
        FakeRequest.result = answer()
        c.update(4)
        self.assertEqual(c.remaining, demo.DECISION_TICKS)
        c.update(5)
        self.assertLess(c.game.velocity, 0)
        self.assertEqual(c.game.ticks, 1)

    def test_explicit_wait_with_tied_probabilities_never_flaps(self):
        c = self.controller
        FakeRequest.result = answer("WAIT", confidence=0.5)
        FakeRequest.result["probabilities"] = {"FLAP": 0.5, "WAIT": 0.5}
        c.update(0)
        c.update(0.1)
        for tick in range(demo.DECISION_TICKS):
            c.update(1 + tick)
        self.assertEqual(c.game.ticks, demo.DECISION_TICKS)
        self.assertGreater(c.game.velocity, 0)
        self.assertEqual(c.trace[-1]["executed_action"], "WAIT")

    def test_flap_is_applied_once_and_trace_is_bounded_credential_free(self):
        c = self.controller
        c.update(0)
        c.update(0.1)
        for tick in range(demo.DECISION_TICKS):
            c.update(1 + tick)
        self.assertAlmostEqual(c.game.velocity, c.game.flap_velocity + demo.DECISION_TICKS * c.game.gravity)
        entry = c.trace[-1]
        for _ in range(100):
            c.trace.append(entry)
        response = c.command("trace")
        self.assertEqual(response["retained"], 32)
        self.assertEqual(response["returned"], 4)
        self.assertFalse(response["complete"])
        self.assertNotIn(CONFIG.key, json.dumps(response))
        self.assertLess(len(json.dumps(response).encode()), 60000)
        self.assertEqual(entry["observation"]["decision_tick"], 0)
        self.assertEqual(entry["response"]["action"], entry["executed_action"])

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
