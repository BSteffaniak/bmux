#!/usr/bin/env python3
"""Real PTY lifecycle regressions for the compatibility harness; no user runtime."""
import importlib.util
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import unittest

sys.dont_write_bytecode = True
DRIVER = Path(__file__).with_name('compat-pty.py')
spec = importlib.util.spec_from_file_location('compat_pty', DRIVER)
m = importlib.util.module_from_spec(spec)
spec.loader.exec_module(m)


class CompatPtyTests(unittest.TestCase):
    def invoke(self, source, timeout=3, marker=True):
        with tempfile.TemporaryDirectory(prefix='compat-test-') as directory:
            root = Path(directory)
            flow = root / 'flow.ok'
            if marker:
                flow.touch()
            code = root / 'child.py'
            code.write_text(source)
            started = time.monotonic()
            result = subprocess.run([sys.executable, str(DRIVER), '--marker', str(flow),
                '--transcript', str(root / 'transcript'), '--timeout', str(timeout),
                '--', sys.executable, str(code)], capture_output=True, timeout=timeout + 5)
            self.assertLess(time.monotonic() - started, timeout + 3)
            transcript = (root / 'transcript').read_bytes()
            if (root / 'pid').exists():
                pid = int((root / 'pid').read_text())
                with self.assertRaises(ProcessLookupError):
                    os.kill(pid, 0)
            return result, transcript

    def test_early_marker_does_not_send_input_before_completed_paint(self):
        result, output = self.invoke("""
import os, select, struct, termios, fcntl, tty, time
from pathlib import Path
tty.setraw(0)
assert os.isatty(0)
assert struct.unpack('HHHH', fcntl.ioctl(0, termios.TIOCGWINSZ, b'\\0'*8))[:2] == (24,80)
assert not select.select([0], [], [], .15)[0], 'input arrived before ready'
os.write(1, b'\\x1b[?2026')
assert not select.select([0], [], [], .15)[0], 'input arrived before complete readiness sequence'
os.write(1, b'l')
# Reproduce attach startup consuming the first key chord, then accepting one.
assert os.read(0, 2) == b'\\x01d'
assert os.read(0, 2) == b'\\x01d'
os.write(1, b'accepted-detach')
""")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn(b'accepted-detach', output)

    def test_missing_marker_or_readiness_fails_closed_and_reaps_child(self):
        for ready, marker in ((True, False), (False, True)):
            source = """
import os, signal, time, tty
from pathlib import Path
Path(__file__).with_name('pid').write_text(str(os.getpid()))
signal.signal(signal.SIGTERM, signal.SIG_IGN)
tty.setraw(0)
""" + ("os.write(1, b'\\x1b[?2026l')\n" if ready else '') + 'time.sleep(60)\n'
            result, _ = self.invoke(source, timeout=.6, marker=marker)
            self.assertEqual(result.returncode, 124)
            self.assertIn(b'deadline exceeded', result.stdout)

    def test_successful_early_exit_is_not_accepted_as_compatibility_success(self):
        result, _ = self.invoke('pass\n')
        self.assertEqual(result.returncode, 1)
        self.assertIn(b'before terminal readiness and flow marker', result.stdout)

    def test_failure_exit_status_is_preserved(self):
        result, _ = self.invoke("""
import os, tty
tty.setraw(0)
os.write(1, b'\\x1b[?2026l')
os.read(0, 2)
raise SystemExit(7)
""")
        self.assertEqual(result.returncode, 7)

    def test_cancellation_reaps_owned_attach(self):
        with tempfile.TemporaryDirectory(prefix='compat-cancel-') as directory:
            root = Path(directory)
            source = root / 'child.py'
            pid_file = root / 'pid'
            source.write_text("import os, time\nfrom pathlib import Path\n"
                              "Path(__file__).with_name('pid').write_text(str(os.getpid()))\n"
                              "time.sleep(60)\n")
            child = subprocess.Popen([sys.executable, str(DRIVER), '--marker', str(root / 'missing'),
                '--transcript', str(root / 'transcript'), '--timeout', '10', '--',
                sys.executable, str(source)], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            try:
                deadline = time.monotonic() + 3
                while not pid_file.exists() and time.monotonic() < deadline:
                    time.sleep(.02)
                self.assertTrue(pid_file.exists())
                pid = int(pid_file.read_text())
                child.terminate()
                self.assertEqual(child.wait(timeout=3), 143)
                with self.assertRaises(ProcessLookupError):
                    os.kill(pid, 0)
            finally:
                if child.poll() is None:
                    child.terminate()
                    child.wait(timeout=3)

    def test_output_is_bounded_not_silently_truncated_as_success(self):
        result, output = self.invoke("import os\nfor _ in range(600): os.write(1, b'x'*8192)\n")
        self.assertEqual(result.returncode, 1)
        self.assertIn(b'output limit exceeded', result.stdout)
        self.assertLessEqual(len(output), m.OUTPUT_LIMIT)


if __name__ == '__main__':
    unittest.main()
