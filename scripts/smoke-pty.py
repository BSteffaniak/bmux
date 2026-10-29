#!/usr/bin/env python3
"""Readiness-driven PTY startup/detach smoke; no startup-delay assumptions."""
import argparse
import errno
import fcntl
import os
from pathlib import Path
import select
import signal
import struct
import subprocess
import termios
import time

READY = b'\x1b[?2026l'  # First completed outer-terminal paint.
LIMIT = 4 * 1024 * 1024


def terminal():
    os.setsid()
    fcntl.ioctl(0, termios.TIOCSCTTY, 0)


def run(command, transcript, timeout, keybind):
    master, slave = os.openpty()
    child = None
    try:
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack('HHHH', 24, 80, 0, 0))
        child = subprocess.Popen(command, stdin=slave, stdout=slave, stderr=slave,
                                 preexec_fn=terminal, close_fds=True)
        os.close(slave)
        slave = None
        deadline = time.monotonic() + timeout
        tail = b''
        ready = sent = opened = False
        total = 0
        next_detach = 0
        with transcript.open('wb') as output:
            while time.monotonic() < deadline:
                readable, _, _ = select.select([master], [], [], min(0.05, max(0, deadline - time.monotonic())))
                data = b''
                if readable:
                    try:
                        data = os.read(master, 65536)
                    except OSError as error:
                        if error.errno != errno.EIO:
                            raise
                    if not data:
                        break
                    total += len(data)
                    if total > LIMIT:
                        print('smoke PTY output limit exceeded', flush=True)
                        return 1
                    output.write(data)
                    output.flush()
                    painted = READY in tail + data
                    tail = (tail + data)[-len(READY):]
                    if painted:
                        if not ready:
                            ready = True
                            if keybind:
                                os.write(master, b'\x01t')
                                opened = True
                                continue
                        elif opened:
                            # Observe the paint caused by the keybind before
                            # detaching, not a fixed sleep after sending it.
                            opened = False
                if ready and not opened and time.monotonic() >= next_detach:
                    os.write(master, b'\x01d')
                    sent = True
                    next_detach = time.monotonic() + 0.5
            else:
                print('smoke PTY readiness/detach deadline exceeded', flush=True)
                return 124
            try:
                code = child.wait(timeout=max(0.01, deadline - time.monotonic()))
            except subprocess.TimeoutExpired:
                print('smoke attach closed PTY without exiting', flush=True)
                return 124
            if not sent:
                print('smoke attach exited before readiness and detach', flush=True)
                return 1
            return code if code >= 0 else 128 - code
    finally:
        try:
            if child is not None and child.poll() is None:
                child.terminate()
                try:
                    child.wait(timeout=0.2)
                except subprocess.TimeoutExpired:
                    child.kill()
                    child.wait(timeout=2)
        finally:
            os.close(master)
            if slave is not None:
                os.close(slave)


def interrupted(signum, _frame):
    raise SystemExit(128 + signum)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--transcript', type=Path, required=True)
    parser.add_argument('--timeout', type=float, required=True)
    parser.add_argument('--keybind', action='store_true')
    parser.add_argument('command', nargs=argparse.REMAINDER)
    args = parser.parse_args()
    if args.timeout <= 0 or len(args.command) < 2 or args.command[0] != '--':
        parser.error('positive timeout and command after -- required')
    for sig in (signal.SIGINT, signal.SIGTERM):
        signal.signal(sig, interrupted)
    raise SystemExit(run(args.command[1:], args.transcript, args.timeout, args.keybind))


if __name__ == '__main__':
    main()
