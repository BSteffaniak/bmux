#!/usr/bin/env python3
"""Bounded black-box acceptance of bmux-bench against an owned synthetic PTY.

This proves harness I/O, artifacts and restoration, NOT emulator performance or
visible presentation. Never attaches to a user BMUX runtime.
"""
import argparse
import errno
import fcntl
import json
import os
from pathlib import Path
import pty
import selectors
import signal
import struct
import subprocess
import tempfile
import termios
import time


def run(binary, output, *, probe=False, cancel=False, mismatch=False, cancel_signal=signal.SIGINT):
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 80, 0, 0))
    original = termios.tcgetattr(slave)
    original[6] = list(original[6])
    args = [binary, "run", "--plain", "--json", "--case",
            "paced-updates" if cancel else "plain-logs", "--samples",
            "100" if cancel else "2", "--output", str(output)]
    if probe:
        args.append("--probe")
    if mismatch:
        args += ["--columns", "81"]

    def own_tty():
        os.setsid()
        fcntl.ioctl(0, termios.TIOCSCTTY, 0)
        os.tcsetpgrp(0, os.getpgrp())

    done_read, done_write = os.pipe()
    release_read, release_write = os.pipe()
    wrapper = "import subprocess,os,sys,signal; [signal.signal(s,lambda *_:None) for s in (signal.SIGINT,signal.SIGTERM,signal.SIGHUP,signal.SIGTSTP)]; code=subprocess.call(sys.argv[3:]); os.write(int(sys.argv[1]),str(code).encode()); os.read(int(sys.argv[2]),1); sys.exit(code)"
    import sys
    child = subprocess.Popen([sys.executable, "-c", wrapper, str(done_write), str(release_read)] + args,
                             stdin=slave, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                             pass_fds=(done_write, release_read), preexec_fn=own_tty)
    os.close(done_write)
    os.close(release_read)
    selector = selectors.DefaultSelector()
    stdout, stderr = bytearray(), bytearray()
    selector.register(master, selectors.EVENT_READ, None)
    selector.register(child.stdout, selectors.EVENT_READ, stdout)
    selector.register(child.stderr, selectors.EVENT_READ, stderr)
    selector.register(done_read, selectors.EVENT_READ, "done")
    received, replies, pending, interrupted = 0, 0, b"", False
    deadline = time.monotonic() + 20
    try:
        while child.poll() is None or any(key.data is not None for key in selector.get_map().values()):
            if time.monotonic() > deadline:
                raise AssertionError("benchmark child exceeded 20-second deadline")
            for key, _ in selector.select(0.05):
                try:
                    block = os.read(key.fd, 65536)
                except OSError as error:
                    if error.errno != errno.EIO:
                        raise
                    block = b""
                if not block:
                    selector.unregister(key.fileobj)
                    continue
                if key.data == "done":
                    actual = termios.tcgetattr(slave)
                    # PENDIN is kernel-maintained pending-input bookkeeping on BSD,
                    # not a mode owned by the benchmark.
                    actual[3] &= ~getattr(termios, "PENDIN", 0)
                    original[3] &= ~getattr(termios, "PENDIN", 0)
                    assert actual == original, f"terminal modes were not restored: {original!r} -> {actual!r}"
                    selector.unregister(done_read)
                    os.write(release_write, b"x")
                    continue
                if key.data is not None:
                    key.data.extend(block)
                    assert len(key.data) < 2_000_000, "unbounded report output"
                    continue
                received += len(block)
                assert received < 20_000_000, "unbounded workload output"
                pending += block
                while b"\x1b[6n" in pending:
                    _, pending = pending.split(b"\x1b[6n", 1)
                    os.write(master, b"\x1b[1;1R")
                    replies += 1
                pending = pending[-3:]
                if cancel and received and not interrupted:
                    os.killpg(child.pid, cancel_signal)
                    interrupted = True
        child.wait(timeout=2)
        assert child.returncode in (0, 1, 130), (child.returncode, stderr.decode(errors="replace"))
        record = json.loads(stdout)
        assert record["status"] == ("cancelled" if cancel else "failed" if mismatch else "complete"), record
        assert (output / "run.json").is_file()
        assert (output / "samples.jsonl").is_file()
        if probe:
            assert replies > 0
            assert any(sample["metric"] == "protocol_round_trip_ns"
                       for outcome in record["outcomes"] for sample in outcome["samples"])
        if mismatch:
            assert received == 0, "geometry mismatch emitted workload bytes"
        return record
    finally:
        if child.poll() is None:
            os.killpg(child.pid, signal.SIGKILL)
            child.wait(timeout=2)
        selector.close()
        child.stdout.close()
        child.stderr.close()
        os.close(master)
        os.close(slave)
        os.close(done_read)
        os.close(release_write)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True, type=lambda value: str(Path(value).resolve()))
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="bmux-bench-pty-") as temporary:
        root = Path(temporary)
        baseline = run(args.binary, root / "baseline")
        run(args.binary, root / "probe", probe=True)
        run(args.binary, root / "cancelled", cancel=True)
        run(args.binary, root / "terminated", cancel=True, cancel_signal=signal.SIGTERM)
        run(args.binary, root / "suspend-cancelled", cancel=True, cancel_signal=signal.SIGTSTP)
        run(args.binary, root / "geometry", mismatch=True)
        for command in (["report", str(root / "baseline"), "--format", "json"],
                        ["compare", str(root / "baseline"), str(root / "baseline"), "--json"]):
            result = subprocess.run([args.binary] + command, capture_output=True, timeout=10)
            assert result.returncode == 0, result.stderr.decode(errors="replace")
            json.loads(result.stdout)
        assert baseline["schema_version"] == 1
    print("PASS: owned PTY run, protocol replies, cancellation, geometry rejection, restoration, artifact/report/compare")


if __name__ == "__main__":
    main()
