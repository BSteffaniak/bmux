#!/usr/bin/env python3
"""Tests for owned-process resource observation (no live runtime required)."""
import errno
import importlib.util
import json
from argparse import Namespace
from unittest import mock
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

SPEC = importlib.util.spec_from_file_location("owned", Path(__file__).with_name("perf-owned-process.py"))
OWNED = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(OWNED)


class ResourceTests(unittest.TestCase):
    def test_recursive_descendants_exclude_foreign_processes(self):
        rows = OWNED.parse_ps("1 0 100 0.0\n3 2 30 4.0\n2 1 20 1.0\n4 0 40 2.0\n")
        self.assertEqual({r[0] for r in OWNED.descendants(rows, 2)}, {2, 3})

    def test_owned_finite_command_and_timeout(self):
        import json
        with tempfile.TemporaryDirectory() as directory:
            for seconds, sleep, expected in [(2, 0.15, 0), (0.1, 5, 124)]:
                output = Path(directory) / f"sample-{seconds}.json"
                completed = subprocess.run([
                    sys.executable, str(Path(__file__).with_name("perf-owned-process.py")),
                    "--seconds", str(seconds), "--interval", "0.05", "--output", str(output),
                    "--", sys.executable, "-c", f"import time; time.sleep({sleep})",
                ], capture_output=True, text=True, timeout=8)
                self.assertEqual(completed.returncode, expected, completed.stderr)
                report = json.loads(output.read_text())
                self.assertEqual(report["timed_out"], expected == 124)
                self.assertGreater(len(report["samples"]), 0)
                self.assertGreater(report["peak_rss_sum_kib"], 0)
                self.assertNotIn("command", report)

    def test_cleanup_kills_child_that_ignores_term_after_parent_exit(self):
        self.check_ignoring_child_cleanup(parent_sleep=0.3, seconds=2, expected=0)

    def test_cleanup_kills_child_that_ignores_term_on_timeout(self):
        self.check_ignoring_child_cleanup(parent_sleep=8, seconds=0.5, expected=124)

    def check_ignoring_child_cleanup(self, parent_sleep, seconds, expected):
        import os
        import time
        with tempfile.TemporaryDirectory() as directory:
            marker = Path(directory) / "child.pid"
            code = (
                "import os,signal,time; "
                "signal.signal(signal.SIGTERM, signal.SIG_IGN); "
                f"open({str(marker)!r},'w').write(str(os.getpid())); "
                "time.sleep(8)"
            )
            parent = (
                "import subprocess,sys,time; "
                f"subprocess.Popen([sys.executable,'-c',{code!r}]); "
                f"time.sleep({parent_sleep})"
            )
            completed = subprocess.run([
                sys.executable, str(Path(__file__).with_name("perf-owned-process.py")),
                "--seconds", str(seconds), "--interval", "0.05", "--output", str(Path(directory) / "sample.json"),
                "--", sys.executable, "-c", parent,
            ], capture_output=True, text=True, timeout=6)
            self.assertEqual(completed.returncode, expected, completed.stderr)
            self.assertTrue(marker.exists())
            pid = int(marker.read_text())
            deadline = time.monotonic() + 2
            while time.monotonic() < deadline:
                status = subprocess.run(["ps", "-o", "stat=", "-p", str(pid)], capture_output=True, text=True)
                if not status.stdout.strip() or status.stdout.strip().startswith("Z"):
                    break
                time.sleep(0.02)
            else:
                # Clean up only this test's explicit child before failing.
                import signal
                os.kill(pid, signal.SIGKILL)
                self.fail("owned child survived process-group cleanup")

    def test_group_permission_error_requires_independent_exit_evidence(self):
        denied = PermissionError(errno.EPERM, "Operation not permitted")
        for rows in ["", "42 Z\n", "99 S\n42 Z+\n"]:
            with self.subTest(rows=rows), mock.patch.object(
                OWNED.os, "killpg", side_effect=denied
            ), mock.patch.object(
                OWNED.subprocess, "run", return_value=mock.Mock(stdout=rows)
            ) as observation, mock.patch.object(OWNED.sys, "stderr") as diagnostics:
                self.assertFalse(OWNED.signal_owned_group(42, 0))
                observation.assert_called_once()
                self.assertTrue(diagnostics.write.called)
        for rows in ["42 S\n", "42 Z\n42 R+\n"]:
            with self.subTest(rows=rows), mock.patch.object(
                OWNED.os, "killpg", side_effect=denied
            ), mock.patch.object(
                OWNED.subprocess, "run", return_value=mock.Mock(stdout=rows)
            ), self.assertRaisesRegex(PermissionError, "live owned group 42"):
                OWNED.signal_owned_group(42, 0)

    def test_permission_evidence_failure_is_not_hidden(self):
        denied = PermissionError(errno.EPERM, "Operation not permitted")
        with mock.patch.object(OWNED.os, "killpg", side_effect=denied), mock.patch.object(
            OWNED.subprocess, "run", side_effect=subprocess.CalledProcessError(1, "ps")
        ), self.assertRaises(subprocess.CalledProcessError):
            OWNED.signal_owned_group(42, 0)
        with mock.patch.object(OWNED.os, "killpg", side_effect=denied), mock.patch.object(
            OWNED.subprocess, "run", return_value=mock.Mock(stdout="invalid observation\n")
        ), self.assertRaises(ValueError):
            OWNED.signal_owned_group(42, 0)

    def test_cleanup_does_not_resignal_a_missing_group(self):
        process = mock.Mock(pid=42)
        with mock.patch.object(OWNED.os, "killpg", side_effect=ProcessLookupError()) as kill:
            OWNED.cleanup(process)
        kill.assert_called_once_with(42, OWNED.signal.SIGTERM)
        process.wait.assert_called_once_with(timeout=3)

    def test_sampling_permission_failure_propagates_after_cleanup(self):
        with tempfile.TemporaryDirectory() as directory:
            process = mock.Mock(pid=42, returncode=0)
            process.poll.return_value = None
            with mock.patch.object(OWNED.subprocess, "Popen", return_value=process), mock.patch.object(
                OWNED.subprocess, "run", side_effect=PermissionError(errno.EPERM, "sampling denied")
            ), mock.patch.object(OWNED, "cleanup") as cleanup, self.assertRaises(PermissionError):
                OWNED.run(Namespace(
                    command=[sys.executable], seconds=1, interval=0.05,
                    output=str(Path(directory) / "sample.json"),
                ))
            cleanup.assert_called_once_with(process)

    def test_workload_output_is_discarded_without_sidecars(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "sample.json"
            status = OWNED.run(Namespace(
                command=[sys.executable, "-c", (
                    "import os; os.write(1,b'private-output'*100000); "
                    "os.write(2,b'private-error'*100000)"
                )],
                seconds=2, interval=0.05, output=str(output),
            ))
            self.assertEqual(status, 0)
            self.assertEqual(list(Path(directory).iterdir()), [output])
            self.assertNotIn("private-output", output.read_text())
            self.assertNotIn("private-error", output.read_text())

    def test_requires_explicit_executable(self):
        from argparse import Namespace
        with self.assertRaisesRegex(ValueError, "absolute executable"):
            OWNED.run(Namespace(command=["python3"], seconds=1, interval=0.2, output="unused"))


if __name__ == "__main__":
    unittest.main()
