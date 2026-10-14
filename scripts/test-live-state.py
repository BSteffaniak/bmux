#!/usr/bin/env python3
"""Offline backend contract tests (not a claim of real AWS conditional-lock proof)."""
import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location('state', Path(__file__).with_name('live-state.py'))
m = importlib.util.module_from_spec(spec)
spec.loader.exec_module(m)


class StateTests(unittest.TestCase):
    def test_state_and_lock_separation(self):
        staging = m.config('staging', 'private-bmux-state', 'us-east-1')
        production = m.config('production', 'private-bmux-state', 'us-east-1')
        self.assertNotEqual(staging['key'], production['key'])
        self.assertTrue(staging['use_lockfile'])
        self.assertTrue(production['encrypt'])
        self.assertFalse(any('secret' in key or 'access' in key for key in staging))

    def test_reject_endpoint_and_environment_injection(self):
        for args in [('dev', 'bucket-name', 'us-east-1'),
                     ('production', 'https://r2.example', 'us-east-1'),
                     ('production', 'bucket-name', 'region\nkey=bad')]:
            with self.assertRaises(ValueError):
                m.config(*args)

    def test_recovery_never_disables_lock(self):
        source = Path(__file__).with_name('live-deploy.sh').read_text()
        self.assertNotIn(' -lock=false', source)
        self.assertNotIn('tofu force-unlock', source)
        self.assertIn('-lock-timeout=5m', source)
        self.assertIn('-lockfile=readonly', source)


if __name__ == '__main__':
    unittest.main()
