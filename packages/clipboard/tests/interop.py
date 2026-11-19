"""Run only inside a disposable desktop/session; the flag does not isolate it.

In each checkout's configured dev shell, build a distinct executable:
  old (pinned shipping SHA, copy ONLY examples/interop_peer.rs into the crate):
    cargo build --locked -p bmux_clipboard --example interop_peer
  candidate:
    cargo build --locked -p bmux_clipboard --example interop_peer --features native-clipboard
Copy each resulting executable before building the next if targets are shared.
From the disposable session, set BMUX_CLIPBOARD_ISOLATED_TEST=1, then run:
  python3 packages/clipboard/tests/interop.py /path/to/old-peer /path/to/new-peer

The default requires persistence after normal owner exit (on Linux, provision
an appropriate clipboard manager). --live-only tests transfers, NOT persistence.
No initial clipboard read or restore. Do not share the display with other tests.
"""
import argparse
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import time

TIMEOUT = 15


def check(reader, case, seed, phase):
    # Never echo arbitrary child output. Use a file instead of waiting for EOF
    # on a pipe that a spawned clipboard helper might have inherited.
    with tempfile.TemporaryFile() as output:
        status = subprocess.run([reader, "check", case, seed], stdin=subprocess.DEVNULL,
                                stdout=output, stderr=subprocess.DEVNULL, timeout=TIMEOUT)
        output.seek(0)
        summary = output.read(80).strip()
    if status.returncode or not re.fullmatch(rb"bytes=\d+ fnv1a64=[0-9a-f]{16}", summary):
        raise RuntimeError(f"{phase}: reader failed (status={status.returncode})")
    print(phase, summary.decode("ascii"), flush=True)


def transfer(writer, reader, case, seed, live_only):
    with tempfile.TemporaryDirectory() as tmp:
        ready = Path(tmp) / "ready"
        with ready.open("wb") as signal:
            owner = subprocess.Popen([writer, "write", case, seed], stdin=subprocess.PIPE,
                                     stdout=signal, stderr=subprocess.DEVNULL)
        try:
            deadline = time.monotonic() + TIMEOUT
            with ready.open("rb") as signal:
                while True:
                    signal.seek(0)
                    if signal.read(8).strip() == b"ready":
                        break
                    if owner.poll() is not None or time.monotonic() >= deadline:
                        raise RuntimeError("writer not ready")
                    time.sleep(0.02)
            check(reader, case, seed, "owner-live")
            if owner.poll() is not None:
                raise RuntimeError("writer exited before release")
            owner.stdin.close()
            if owner.wait(timeout=TIMEOUT) != 0:
                raise RuntimeError("writer exit failed")
            if not live_only:
                check(reader, case, seed, "after-exit")
        finally:
            if owner.poll() is None:
                owner.kill()
            owner.stdin.close()
            owner.wait(timeout=TIMEOUT)


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("old", type=Path)
    parser.add_argument("candidate", type=Path)
    parser.add_argument("--live-only", action="store_true")
    args = parser.parse_args()
    if os.environ.get("BMUX_CLIPBOARD_ISOLATED_TEST") != "1":
        parser.error("requires BMUX_CLIPBOARD_ISOLATED_TEST=1 on a disposable desktop")
    old, candidate = args.old.resolve(strict=True), args.candidate.resolve(strict=True)
    if old.samefile(candidate):
        parser.error("provide distinct shipping and candidate executables")
    for direction, writer, reader in [("old->candidate", old, candidate), ("candidate->old", candidate, old)]:
        for index, case in enumerate(["text", "empty", "image"]):
            print(direction, case, flush=True)
            transfer(writer, reader, case, str(index + (10 if writer == old else 20)), args.live_only)


if __name__ == "__main__":
    try:
        main()
    except (OSError, RuntimeError, subprocess.TimeoutExpired) as error:
        sys.exit(str(error))
