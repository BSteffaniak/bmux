#!/usr/bin/env python3
"""Browser startup regressions; optional real Chromium probe uses only owned state."""
import argparse
import importlib.util
from pathlib import Path
import tempfile
import unittest
from unittest.mock import Mock

spec = importlib.util.spec_from_file_location('native_acceptance',
                                               Path(__file__).with_name('live-native-acceptance.py'))
NATIVE = importlib.util.module_from_spec(spec)
spec.loader.exec_module(NATIVE)
BROWSER = None


class BrowserStartupTests(unittest.TestCase):
    def test_terminal_acceptance_checks_visibility_not_status_wording(self):
        harness = object.__new__(NATIVE.Harness)
        harness.cdp = Mock()
        harness.cdp.evaluate.return_value = True
        harness.inspect = Mock()
        self.assertTrue(harness.inspect_terminal())
        expression = harness.cdp.evaluate.call_args.args[0]
        self.assertNotIn('textContent', expression)
        harness.inspect.assert_called_once_with('NATIVE_TERMINAL_PUBLIC_5831',
                                               ['NATIVE_AUDIENCE_PUBLIC_5831', 'NATIVE_SPEAKER_PRIVATE_5831'])
        harness.cdp.evaluate.return_value = False
        harness.inspect.reset_mock()
        self.assertFalse(harness.inspect_terminal())
        harness.inspect.assert_not_called()
        harness.cdp.evaluate.return_value = True
        harness.inspect.side_effect = AssertionError('missing audience text')
        self.assertFalse(harness.inspect_terminal())

    def test_early_exit_is_not_reported_as_timeout_or_ready(self):
        child = Mock()
        child.poll.return_value = -6
        path = Mock()
        path.exists.return_value = True
        with self.assertRaisesRegex(AssertionError, 'exited before readiness \\(code -6\\)'):
            NATIVE.browser_ready(child, path)
        path.exists.assert_not_called()

    def test_live_process_requires_readiness_file(self):
        child = Mock()
        child.poll.return_value = None
        path = Mock()
        for ready in (False, True):
            path.exists.return_value = ready
            self.assertEqual(NATIVE.browser_ready(child, path), ready)

    def test_browser_environment_preserves_all_other_isolation(self):
        original = {'HOME': '/owned/home', 'TMPDIR': '/very/long/owned/tmp',
                    'BMUX_RUNTIME_DIR': '/owned/runtime', 'XDG_CONFIG_HOME': '/owned/config'}
        result = NATIVE.browser_environment(original, '/tmp/owned-short')
        self.assertEqual(result, dict(original, TMPDIR='/tmp/owned-short'))
        self.assertEqual(original['TMPDIR'], '/very/long/owned/tmp')

    def test_real_browser_starts_with_deep_runner_paths(self):
        if BROWSER is None:
            self.skipTest('pass --browser for real locked Chromium regression')
        # Before the fix this TMPDIR yields a fatal "Socket path too long".
        with tempfile.TemporaryDirectory(prefix='bmux-browser-test-') as outer:
            root = Path(outer) / ('runner-' + 'x' * 90)
            root.mkdir()
            args = argparse.Namespace(binary=Path('/unused-test-binary'))
            harness = NATIVE.Harness(args, root)
            try:
                harness.browser_temp = tempfile.TemporaryDirectory(prefix='bmux-cdp-', dir='/tmp')
                env = NATIVE.browser_environment(harness.env, harness.browser_temp.name)
                self.assertGreater(len(harness.env['TMPDIR'].encode()), 108)
                self.assertEqual(env['HOME'], str(root / 'home'))
                self.assertEqual(env['BMUX_RUNTIME_DIR'], str(root / 'runtime'))
                profile = root / 'browser-profile'
                child = harness.spawn([str(BROWSER), '--headless', '--no-sandbox', '--disable-gpu',
                    '--disable-background-networking', '--no-first-run', '--no-default-browser-check',
                    '--remote-debugging-port=0', '--user-data-dir=' + str(profile), 'about:blank'], env=env)
                NATIVE.wait('deep-path browser', lambda: NATIVE.browser_ready(child, profile / 'DevToolsActivePort'))
                self.assertTrue(profile.is_dir())
                short = Path(harness.browser_temp.name)
                self.assertTrue(short.is_dir())
                self.assertEqual(short.stat().st_mode & 0o777, 0o700)
            finally:
                harness.close()
            self.assertIsNotNone(child.poll())
            self.assertFalse(short.exists())


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--browser', type=Path)
    args, remaining = parser.parse_known_args()
    BROWSER = args.browser.resolve() if args.browser else None
    unittest.main(argv=[__file__, *remaining])
