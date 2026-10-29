#!/usr/bin/env python3
"""Sample an explicitly launched process tree; never attach to an existing runtime.

RSS is a sum of per-process resident sizes (shared pages may be counted twice).
ps %CPU is an OS-reported gauge, not CPU time or energy. GPU memory is not RSS.
Workload stdin/stdout/stderr use DEVNULL: no workload output is captured.
Children that leave the owned process group cannot be cleaned up by this tool.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import time


def descendants(rows, root):
    selected = {root}
    while True:
        added = {pid for pid, parent, _, _ in rows if parent in selected}
        expanded = selected | added
        if expanded == selected:
            return [row for row in rows if row[0] in selected]
        selected = expanded


def parse_ps(text):
    rows = []
    for line in text.splitlines():
        fields = line.split()
        if len(fields) == 4:
            rows.append((int(fields[0]), int(fields[1]), int(fields[2]), float(fields[3])))
    return rows


def signal_owned_group(group, sig):
    """Signal our group; distinguish a dead-group race from denied live cleanup."""
    try:
        os.killpg(group, sig)
    except ProcessLookupError:
        return False
    except PermissionError as error:
        # macOS can report EPERM for a group containing only exiting/zombie
        # members. Do not treat EPERM itself as proof of exit: independently
        # check membership, and preserve permission failures for live groups.
        observed = subprocess.run(
            ["ps", "-axo", "pgid=,stat="],
            capture_output=True, text=True, timeout=5, check=True,
            env={**os.environ, "LC_ALL": "C"},
        )
        for line in observed.stdout.splitlines():
            fields = line.split()
            if len(fields) != 2:
                raise ValueError("invalid process-group cleanup observation") from error
            if int(fields[0]) == group and not fields[1].startswith("Z"):
                raise PermissionError(
                    error.errno, f"permission denied signalling live owned group {group}"
                ) from error
        print(
            f"resource sampling cleanup: EPERM for group {group}; "
            "independent ps observation found no live members",
            file=sys.stderr,
        )
        return False
    return True


def cleanup(process):
    # Root exit does not prove its children exited. Keep the root unreaped
    # during timeout cleanup, and escalate the group independently of its wait.
    if signal_owned_group(process.pid, signal.SIGTERM):
        deadline = time.monotonic() + 0.5
        while signal_owned_group(process.pid, 0):
            if time.monotonic() >= deadline:
                signal_owned_group(process.pid, signal.SIGKILL)
                break
            time.sleep(0.02)
    process.wait(timeout=3)


def run(args):
    if not args.command or not 0 < args.seconds <= 600 or not 0.05 <= args.interval <= 5:
        raise ValueError("require command, 0 < seconds <= 600, and 0.05 <= interval <= 5")
    executable = Path(args.command[0])
    if not executable.is_absolute() or not executable.is_file():
        raise ValueError("command must use an explicit absolute executable path")
    output = Path(args.output)
    output.parent.mkdir(parents=True, exist_ok=True)
    # Exclusive creation prevents accidentally replacing previous evidence.
    with output.open("x") as report:
        identity = hashlib.sha256(executable.read_bytes()).hexdigest()
        started = time.monotonic()
        process = subprocess.Popen(
            args.command, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL, start_new_session=True,
        )
        samples = []
        timed_out = False
        sample_errors = 0
        try:
            while process.poll() is None:
                if time.monotonic() - started >= args.seconds:
                    timed_out = True
                    break
                observed = subprocess.run(
                    ["ps", "-axo", "pid=,ppid=,rss=,pcpu="],
                    capture_output=True, text=True, timeout=5, check=True,
                    env={**os.environ, "LC_ALL": "C"},
                )
                try:
                    rows = descendants(parse_ps(observed.stdout), process.pid)
                except ValueError:
                    sample_errors += 1
                    rows = []
                samples.append({
                    "elapsed_s": time.monotonic() - started,
                    "process_count": len(rows),
                    "rss_sum_kib": sum(row[2] for row in rows),
                    "cpu_percent_sum": sum(row[3] for row in rows),
                })
                time.sleep(args.interval)
        finally:
            cleanup(process)
            json.dump({
                "schema_version": 1,
                "executable": str(executable),
                "executable_sha256": identity,
                "duration_s": time.monotonic() - started,
                "interval_s": args.interval,
                "returncode": process.returncode,
                "timed_out": timed_out,
                "sample_errors": sample_errors,
                "peak_rss_sum_kib": max((s["rss_sum_kib"] for s in samples), default=0),
                "samples": samples,
                "limitations": [
                    "RSS sums may double-count shared pages; not physical footprint or GPU memory",
                    "ps CPU percentages are OS gauges, not CPU time or energy",
                    "short-lived or reparented descendants can escape sampling",
                    "sampling overhead is not subtracted; compare identical sampling configurations",
                    "children that leave the owned process group escape cleanup",
                    "workload stdin/stdout/stderr discarded, not captured",
                    "no workload content, command arguments, or environment exported",
                ],
            }, report, indent=2)
            report.write("\n")
    return 124 if timed_out else process.returncode


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--seconds", type=float, default=30)
    parser.add_argument("--interval", type=float, default=0.2)
    parser.add_argument("--output", required=True)
    parser.add_argument("command", nargs=argparse.REMAINDER)
    args = parser.parse_args()
    if args.command[:1] == ["--"]:
        args.command = args.command[1:]
    try:
        return run(args)
    except (ValueError, OSError, subprocess.SubprocessError) as error:
        parser.exit(1, f"resource sampling failed: {error}\n")


if __name__ == "__main__":
    raise SystemExit(main())
