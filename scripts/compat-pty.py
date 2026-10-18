#!/usr/bin/env python3
"""Bounded, isolated PTY driver for terminal compatibility scenarios.

A scenario marker alone does not mean attach is ready: short-lived shells can
finish while attach is still starting. Wait for a completed outer-terminal paint,
then retry the normal-mode detach chord until exit or the hard deadline.
"""
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

READY = b'\x1b[?2026l'
OUTPUT_LIMIT = 4 * 1024 * 1024


def controlling_terminal():
    os.setsid()
    fcntl.ioctl(0, termios.TIOCSCTTY, 0)


def stop_group(child):
    # The attach and its foreground descendants belong to this freshly created
    # session. The isolated BMUX daemon is separately stopped by compat-matrix.
    for sig in (signal.SIGTERM, signal.SIGKILL):
        try:
            os.killpg(child.pid, sig)
        except ProcessLookupError:
            break
        if sig == signal.SIGTERM:
            time.sleep(0.1)
    child.wait(timeout=2)


def run(command, marker, transcript, timeout):
    master, slave = os.openpty()
    child = None
    try:
        # script(1) inherits a zero-sized window on a non-interactive CI runner.
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack('HHHH', 24, 80, 0, 0))
        child = subprocess.Popen(command, stdin=slave, stdout=slave, stderr=slave,
                                 preexec_fn=controlling_terminal, close_fds=True)
        os.close(slave)
        slave = None
        deadline = time.monotonic() + timeout
        ready = False
        next_detach = 0
        sent = False
        tail = b''
        total = 0
        with transcript.open('wb') as output:
            while True:
                if time.monotonic() >= deadline:
                    print('compatibility PTY deadline exceeded; terminating owned attach', flush=True)
                    return 124
                readable, _, _ = select.select([master], [], [], 0.05)
                if readable:
                    try:
                        data = os.read(master, 65536)
                    except OSError as exc:
                        if exc.errno != errno.EIO:
                            raise
                        data = b''  # Linux reports EIO at slave close; BSD reports EOF.
                    if not data:
                        break
                    total += len(data)
                    if total > OUTPUT_LIMIT:
                        print('compatibility PTY output limit exceeded', flush=True)
                        return 1
                    output.write(data)
                    output.flush()
                    ready = ready or READY in tail + data
                    tail = (tail + data)[-len(READY):]
                if ready and marker.is_file() and time.monotonic() >= next_detach:
                    # Startup can consume input before the attach input loop is
                    # active. Retrying this idempotent detach request is bounded
                    # by the same deadline, not an arbitrary startup sleep.
                    os.write(master, b'\x01d')
                    sent = True
                    next_detach = time.monotonic() + 0.5
            # Closing the PTY is not sufficient proof of process completion.
            remaining = max(0.01, deadline - time.monotonic())
            try:
                code = child.wait(timeout=remaining)
            except subprocess.TimeoutExpired:
                print('compatibility attach closed PTY but did not exit', flush=True)
                return 124
            if not sent:
                print('compatibility attach exited before terminal readiness and flow marker', flush=True)
                return 1
            return code if code >= 0 else 128 - code
    finally:
        if child is not None:
            stop_group(child)
        os.close(master)
        if slave is not None:
            os.close(slave)


def interrupted(signum, _frame):
    raise SystemExit(128 + signum)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--marker', type=Path, required=True)
    parser.add_argument('--transcript', type=Path, required=True)
    parser.add_argument('--timeout', type=float, required=True)
    parser.add_argument('command', nargs=argparse.REMAINDER)
    args = parser.parse_args()
    if args.timeout <= 0 or not args.command or args.command[0] != '--' or len(args.command) < 2:
        parser.error('positive timeout and command after -- required')
    for sig in (signal.SIGTERM, signal.SIGINT):
        signal.signal(sig, interrupted)
    raise SystemExit(run(args.command[1:], args.marker, args.transcript, args.timeout))


if __name__ == '__main__':
    main()
