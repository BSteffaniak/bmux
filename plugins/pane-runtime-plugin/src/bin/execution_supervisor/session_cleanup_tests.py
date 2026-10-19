#!/usr/bin/env python3
"""Standalone POSIX identity proof; only freshly created private sessions signaled.

Run: python3 plugins/pane-runtime-plugin/src/bin/execution_supervisor/session_cleanup_tests.py
This checks kernel assumptions, not the Rust backend or Linux cgroup containment.
"""
import os
import signal
import subprocess
import sys


def join(group):
    try:
        os.setpgid(0, group)
    except PermissionError:
        sys.exit(2)
    os.killpg(os.getpgrp(), signal.SIGKILL)


def private_session(unrelated_group):
    # Popen(start_new_session=True) created this private identity anchor.
    os.set_inheritable(1, False)
    target = subprocess.Popen(
        [sys.executable, "-c", "import signal,time; signal.signal(signal.SIGHUP,signal.SIG_IGN); signal.signal(signal.SIGTERM,signal.SIG_IGN); print('ready',flush=True); time.sleep(60)"],
        process_group=0,
        stdout=subprocess.PIPE,
        text=True,
    )
    assert target.stdout.readline().strip() == "ready"
    try:
        rejected = subprocess.run([sys.executable, __file__, "join", str(unrelated_group)], check=False)
        assert rejected.returncode == 2, rejected.returncode
        killed = subprocess.run([sys.executable, __file__, "join", str(target.pid)], check=False)
        assert killed.returncode == -signal.SIGKILL, killed.returncode
        assert target.wait(timeout=5) == -signal.SIGKILL
    finally:
        if target.poll() is None:
            target.kill()
            target.wait(timeout=5)


if __name__ == "__main__":
    if len(sys.argv) == 3:
        if sys.argv[1] == "join":
            join(int(sys.argv[2]))
        else:
            private_session(int(sys.argv[2]))
    else:
        subprocess.run(
            [sys.executable, __file__, "session", str(os.getpgrp())],
            start_new_session=True,
            check=True,
            timeout=15,
        )
        print("PASS: same-session group join pins identity; unrelated group rejected; HUP/TERM-ignoring target killed")
