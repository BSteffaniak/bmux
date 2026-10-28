#!/usr/bin/env python3
"""Offline guards for private monitoring; no containers, credentials or cloud IO."""
import importlib.util
import json
import re
from pathlib import Path
import unittest

ROOT = Path(__file__).resolve().parent.parent
MONITORING = ROOT / "infra/live-monitoring"


def load(name):
    return json.loads((MONITORING / name).read_text())


class MonitoringTests(unittest.TestCase):
    def test_services_are_private_and_persistent(self):
        compose = load("compose.json")
        self.assertEqual(set(compose["services"]), {"prometheus", "alertmanager", "grafana"})
        for service in compose["services"].values():
            self.assertIn(":?Run scripts/check-live-monitoring-images.py", service["image"])
            self.assertEqual(service["restart"], "unless-stopped")
            self.assertTrue(all(p.startswith("127.0.0.1:") for p in service["ports"]))
            self.assertTrue(any(not v.startswith(".") for v in service["volumes"]))
            self.assertIn("no-new-privileges:true", service["security_opt"])
        args = compose["services"]["prometheus"]["command"]
        self.assertIn("--storage.tsdb.retention.time=30d", args)
        self.assertIn("--storage.tsdb.retention.size=5GB", args)
        self.assertEqual(len(compose["secrets"]), 3)
        for secret in compose["secrets"].values():
            self.assertTrue(secret["file"].startswith("${MONITORING_"))
            self.assertIn(":?", secret["file"])

    def test_images_are_digest_pinned_and_scanner_fails_closed(self):
        images = load("images.json")
        spec = importlib.util.spec_from_file_location("scan", ROOT / "scripts/check-live-monitoring-images.py")
        scan = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(scan)
        calls = []
        class Result:
            returncode = 0
        def passing(args, **kwargs):
            calls.append(args)
            self.assertIn("--exit-code", args)
            self.assertIn("HIGH,CRITICAL", args)
            self.assertNotIn("--ignore-unfixed", args)
            self.assertFalse(any(k.startswith("TRIVY_") for k in kwargs["env"]))
            return Result()
        env = scan.checked_images(images, passing)
        self.assertEqual(len(calls), 3)
        for name, image in images.items():
            self.assertRegex(image, r"@sha256:[0-9a-f]{64}$")
            self.assertEqual(env["MONITORING_" + name.upper() + "_IMAGE"], image)
        def failing(*args, **kwargs):
            result = Result()
            result.returncode = 1
            return result
        with self.assertRaises(RuntimeError):
            scan.checked_images(images, failing)
        with self.assertRaises(ValueError):
            scan.checked_images({**images, "grafana": "grafana/grafana:latest"}, passing)

    def test_scrape_uses_separate_file_credential_without_redirects(self):
        config = load("prometheus.json")
        scrape, = config["scrape_configs"]
        self.assertEqual(scrape["scheme"], "https")
        self.assertFalse(scrape["follow_redirects"])
        self.assertEqual(scrape["metrics_path"], "/metrics")
        self.assertEqual(scrape["authorization"], {
            "type": "Bearer", "credentials_file": "/run/secrets/relay_metrics_key"})
        self.assertNotIn("publisher", json.dumps(config))
        self.assertIn("/etc/prometheus/alerts.json", config["rule_files"])

    def test_alerts_never_equate_idle_with_failure(self):
        rules = load("alerts.json")["groups"][0]["rules"]
        self.assertGreaterEqual(len(rules), 3)
        relay_source = (ROOT / "packages/live-relay/src/main.rs").read_text()
        relay_diagnostics = ROOT / "packages/live-relay/src/observability.rs"
        if relay_diagnostics.exists():
            relay_source += relay_diagnostics.read_text()
        for rule in rules:
            self.assertNotIn("frames_total", rule["expr"])
            self.assertNotIn("last_frame", rule["expr"])
            self.assertIn("summary", rule["annotations"])
            for metric in re.findall(r"bmux_relay_[a-z_]+", rule["expr"]):
                self.assertIn(metric, relay_source)

    def test_alerts_have_real_file_based_receiver(self):
        config = load("alertmanager.json")
        receiver, = config["receivers"]
        self.assertEqual(config["route"]["receiver"], receiver["name"])
        webhook, = receiver["webhook_configs"]
        self.assertEqual(webhook["url_file"], "/run/secrets/alert_webhook")
        self.assertTrue(webhook["send_resolved"])
        self.assertNotIn("url", webhook)

    def test_dashboard_queries_existing_metrics(self):
        dashboard = load("grafana/dashboards/relay.json")
        self.assertEqual(dashboard["uid"], "bmux-live")
        self.assertGreaterEqual(len(dashboard["panels"]), 6)
        self.assertIn("healthy idle", dashboard["description"])
        for panel in dashboard["panels"]:
            self.assertEqual(panel["datasource"]["uid"], "bmux-relay")
            self.assertTrue(panel["targets"][0]["expr"])


if __name__ == "__main__":
    unittest.main()
