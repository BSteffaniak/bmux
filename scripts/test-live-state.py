#!/usr/bin/env python3
"""Offline backend contract tests (not a claim of real R2 conditional-lock proof)."""
import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location('state', Path(__file__).with_name('live-state.py'))
m = importlib.util.module_from_spec(spec)
spec.loader.exec_module(m)


class StateTests(unittest.TestCase):
    def test_state_and_lock_separation(self):
        staging = m.config('staging', 'private-bmux-state', 'auto', 'a' * 32)
        production = m.config('production', 'private-bmux-state', 'auto', 'a' * 32)
        self.assertNotEqual(staging['key'], production['key'])
        self.assertTrue(staging['use_lockfile'])
        self.assertEqual(production['endpoint'], 'https://' + 'a' * 32 + '.r2.cloudflarestorage.com')
        self.assertFalse(any('secret' in key or 'access' in key for key in staging))

    def test_reject_endpoint_and_environment_injection(self):
        for args in [('dev', 'bucket-name', 'auto'),
                     ('production', 'https://r2.example', 'auto'),
                     ('production', 'bucket-name', 'region\nkey=bad')]:
            with self.assertRaises(ValueError):
                m.config(*args, 'a' * 32)

    def test_recovery_never_disables_lock(self):
        source = Path(__file__).with_name('live-deploy.sh').read_text()
        # Inspection alone avoids backend writes, including lock acquisition.
        # Keep the exception bounded to its exact branch and require it to exit
        # before any deployment/recovery code can execute without locking.
        before, marker, remainder = source.partition('if [[ "$MODE" == inspect ]]; then\n')
        self.assertTrue(marker)
        inspection, end, after = remainder.partition('\nfi\n')
        self.assertTrue(end)
        self.assertEqual(inspection.count(' -lock=false'), 1)
        self.assertIn('tofu plan -json -input=false -lock=false ', inspection)
        self.assertTrue(inspection.endswith('  exit 0'))
        self.assertNotIn('tofu apply', inspection)
        self.assertNotIn('tofu state', inspection)
        self.assertNotIn('tofu import', inspection)
        self.assertNotIn(' -lock=false', before + after)
        self.assertNotIn('tofu force-unlock', source)
        self.assertIn('-lock-timeout=5m', source)
        self.assertIn('-lockfile=readonly', source)


if __name__ == '__main__':
    unittest.main()
