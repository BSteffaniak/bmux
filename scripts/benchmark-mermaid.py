#!/usr/bin/env python3
"""Dedicated Linux benchmark runner; never builds or modifies the checkout.

Standard library only. Launch only after the coordinator grants capacity.
Raw evidence precedes summaries. Resource sampling is explicitly approximate.
"""
import argparse
from concurrent.futures import ThreadPoolExecutor
import difflib
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import random
import selectors
import signal
import statistics
import subprocess
import time
import xml.etree.ElementTree as ET

ROOT = Path(__file__).resolve().parents[1]
CORPUS = ROOT / "packages/mermaid-render/bench-corpus"
MODES = ("current", "worker", "native", "wasm-cold", "wasm-warm", "wasm-jit-cold", "wasm-jit-warm", "wasm-fuel")


def sha(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def command(*args):
    result = subprocess.run(args, cwd=ROOT, text=True, capture_output=True, timeout=10, check=False)
    return {"argv": args, "exit": result.returncode, "stdout": result.stdout, "stderr": result.stderr}


def text(path):
    try:
        return Path(path).read_text()
    except OSError:
        return None


def corpus(output):
    manifest = json.loads((CORPUS / "manifest.json").read_text())
    sources = {path.stem: path for path in CORPUS.glob("*.mmd")}
    generated = output / "generated"
    generated.mkdir()
    dense = "flowchart LR\n" + "".join(f"N{i} --> N{j}\n" for i in range(32)
                                            for j in range(i + 1, 32) if (i + j) % 3 == 0)
    deep = "flowchart TD\n" + "".join(f"subgraph S{i}\n" for i in range(24))
    deep += "A --> B\n" + "end\n" * 24
    base = "flowchart LR\nA --> B\n%% "
    data = {"dense": dense, "deep": deep, "directive": "%%{init: {}}%%\nflowchart LR\nA --> B\n"}
    for name, size in (("near-limit", 65535), ("over-limit", 65537)):
        data[name] = base + "x" * (size - len(base.encode()))
    for name, source in data.items():
        path = generated / f"{name}.mmd"
        path.write_text(source)
        sources[name] = path
    return manifest, sources


def process_tree(pid):
    """Sample RSS and cumulative CPU of the currently visible process tree.

    Fast-exiting children may be missed; CPU authority is wait4 at trial end.
    RSS sum is not unique physical memory (shared pages may be double counted).
    """
    pending, seen, rss, cpu = [pid], set(), 0, 0
    ticks = os.sysconf("SC_CLK_TCK")
    while pending:
        current = pending.pop()
        if current in seen:
            continue
        seen.add(current)
        try:
            stat = Path(f"/proc/{current}/stat").read_text().rsplit(")", 1)[1].split()
            cpu += (int(stat[11]) + int(stat[12])) / ticks
            status = Path(f"/proc/{current}/status").read_text()
            rss += next((int(line.split()[1]) * 1024 for line in status.splitlines()
                         if line.startswith("VmRSS:")), 0)
            tasks = Path(f"/proc/{current}/task")
            for task in tasks.iterdir():
                children = text(task / "children")
                if children:
                    pending.extend(map(int, children.split()))
        except (OSError, ValueError, ProcessLookupError):
            continue
    return {"rss_sum_bytes": rss, "visible_cpu_seconds": cpu, "visible_processes": len(seen)}


class Trial:
    def __init__(self, binary, worker, mode, timeout, sample_ms, fuel):
        self.started = time.monotonic_ns()
        self.process = subprocess.Popen([str(binary), mode, str(worker), str(fuel)], stdin=subprocess.PIPE,
                                        stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                                        start_new_session=True, bufsize=0)
        self.selector = selectors.DefaultSelector()
        self.selector.register(self.process.stdout, selectors.EVENT_READ)
        self.buffer = b""
        self.timeout = timeout
        self.interval = sample_ms / 1000
        self.samples = []
        self.ready = self.read("READY")
        self.startup_ns = time.monotonic_ns() - self.started

    def resources(self):
        pid = self.process.pid
        snapshot = process_tree(pid)
        try:
            snapshot["host_fds"] = len(list(Path(f"/proc/{pid}/fd").iterdir()))
            snapshot["host_threads"] = len(list(Path(f"/proc/{pid}/task").iterdir()))
        except OSError:
            snapshot["host_fds"] = snapshot["host_threads"] = None
        return snapshot

    def read(self, expected):
        deadline = time.monotonic() + self.timeout
        while b"\n" not in self.buffer:
            if time.monotonic() >= deadline:
                self.close(force=True)
                raise TimeoutError(f"driver {expected} deadline")
            self.samples.append(process_tree(self.process.pid))
            if self.selector.select(self.interval):
                chunk = os.read(self.process.stdout.fileno(), 8192)
                if not chunk:
                    self.close(force=True)
                    raise RuntimeError(f"driver exited before {expected}")
                self.buffer += chunk
                if len(self.buffer) > 65536:
                    self.close(force=True)
                    raise RuntimeError("oversized driver record")
        line, self.buffer = self.buffer.split(b"\n", 1)
        fields = line.decode().split("\t")
        if fields[0] != expected:
            self.close(force=True)
            raise RuntimeError(f"unexpected driver record: {fields[0]}")
        return fields

    def render(self, source, cancel, svg):
        before = len(self.samples)
        started = time.monotonic_ns()
        self.process.stdin.write(f"{source}\t{cancel}\t{svg}\n".encode())
        fields = self.read("RESULT")
        samples = self.samples[before:]
        total_ns, constructor_ns, render_call_ns = int(fields[1]), int(fields[6]), int(fields[7])
        return {"render_ns": total_ns, "constructor_ns": constructor_ns, "render_call_ns": render_call_ns,
                "teardown_and_timer_overhead_ns": max(0, total_ns - constructor_ns - render_call_ns),
                "output_bytes": int(fields[2]),
                "svg_sha256": fields[3], "status": fields[4],
                "cancel_return_ns": None if fields[5] == "-" else int(fields[5]),
                "driver_roundtrip_ns": time.monotonic_ns() - started,
                "sampled_peak_tree_rss_bytes": max((s["rss_sum_bytes"] for s in samples), default=None),
                "retained_resources": self.resources(),
                "actual_stop_ns": None,
                "actual_stop_note": "return latency only; hard-stop contract needs separate verification"}

    def close(self, force=False):
        if hasattr(self, "closed"):
            return self.closed
        self.process.stdin.close()
        deadline = time.monotonic() + self.timeout
        while True:
            pid, status, usage = os.wait4(self.process.pid, os.WNOHANG)
            if pid:
                break
            if force or time.monotonic() >= deadline:
                try:
                    os.killpg(self.process.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                pid, status, usage = os.wait4(self.process.pid, 0)
                break
            time.sleep(self.interval)
        self.process.returncode = os.waitstatus_to_exitcode(status)
        self.selector.close()
        self.closed = {"exit": self.process.returncode, "wall_ns": time.monotonic_ns() - self.started,
                       "host_and_waited_child_cpu_seconds": usage.ru_utime + usage.ru_stime,
                       "wait4_maxrss_bytes": usage.ru_maxrss * 1024,
                       "sampled_peak_tree_rss_bytes": max((s["rss_sum_bytes"] for s in self.samples), default=None)}
        return self.closed


def svg_info(path):
    try:
        root = ET.parse(path).getroot()
        fonts = sorted({value for node in root.iter() for key, value in node.attrib.items()
                        if "font" in key} | {node.attrib["style"] for node in root.iter()
                                              if "font" in node.attrib.get("style", "")} |
                       {node.text for node in root.iter() if node.tag.endswith("}style") and node.text})
        return {"width": root.get("width"), "height": root.get("height"),
                "viewBox": root.get("viewBox"), "fonts": fonts}
    except (OSError, ET.ParseError) as error:
        return {"parse_error": str(error)}


def quantile(values, fraction):
    values = sorted(values)
    if fraction == .5:
        return statistics.median(values) if values else None
    return values[max(0, math.ceil(len(values) * fraction) - 1)] if values else None


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--worker", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True, help="new evidence directory outside checkout")
    parser.add_argument("--modes", nargs="+", choices=MODES, default=list(MODES))
    parser.add_argument("--trials", type=int, default=10)
    parser.add_argument("--rounds", type=int, default=5)
    parser.add_argument("--cases", nargs="+", help="explicit corpus subset; native still restricted to trusted diagnostic intersection")
    parser.add_argument("--fuel", type=int, default=1_000_000_000, help="positive per-render wasm-fuel budget; not calibrated")
    parser.add_argument("--concurrency", type=int, choices=range(1, 5), default=1, help="1..4 independent host replicas; NOT shared-module async contention")
    parser.add_argument("--seed", type=int, default=20261014)
    parser.add_argument("--timeout", type=float, default=15)
    parser.add_argument("--sample-ms", type=float, default=5)
    parser.add_argument("--phase", choices=("correctness", "timing", "cancellation"), required=True)
    parser.add_argument("--cancel-ms", type=int, default=5)
    parser.add_argument("--cache-settings", required=True, help="page/module/font caches, affinity, governor, load")
    parser.add_argument("--build-record", type=Path, required=True, help="JSON: commands/config, elapsed/CPU/peak memory, artifacts; null unknowns")
    parser.add_argument("--artifact", type=Path, action="append", default=[], help="all deployment files including host/runtime and guest")
    args = parser.parse_args()
    if platform.system() != "Linux":
        parser.error("resource accounting currently Linux-only")
    if min(args.trials, args.rounds, args.timeout, args.sample_ms) <= 0 or args.cancel_ms < 0:
        parser.error("trials/rounds/timeout/sample interval must be positive; cancel delay nonnegative")
    if not 0 < args.fuel <= 2**64 - 1:
        parser.error("fuel must fit positive u64")
    if args.concurrency > 1 and (args.phase == "correctness" or "native" in args.modes):
        parser.error("concurrent replicas exclude native diagnostics and correctness; choose explicit modes")
    args.output = args.output.resolve()
    if args.output == ROOT or ROOT in args.output.parents:
        parser.error("evidence output must be outside the shared checkout")
    args.binary, args.worker = args.binary.resolve(), args.worker.resolve()
    if any(any(char in str(path) for char in "\t\n\r") for path in (args.output, args.binary, args.worker, CORPUS)):
        parser.error("line-protocol paths cannot contain tabs or newlines")
    args.output.mkdir(parents=True, exist_ok=False)
    manifest, sources = corpus(args.output)
    if args.cases and not set(args.cases) <= set(sources):
        parser.error("unknown corpus case")
    metadata = {"schema": 1, "acceptance": "prototype; unmeasured until evidence reviewed",
                "arguments": {key: str(value) if isinstance(value, Path) else value for key, value in vars(args).items() if key != "artifact"},
                "platform": platform.platform(), "cpu": text("/proc/cpuinfo"),
                "affinity": sorted(os.sched_getaffinity(0)), "clock": time.get_clock_info("monotonic")._asdict() if hasattr(time.get_clock_info("monotonic"), "_asdict") else str(time.get_clock_info("monotonic")),
                "git": command("git", "rev-parse", "HEAD"), "dirty": command("git", "status", "--porcelain"),
                "rustc": command("rustc", "-Vv"), "cargo": command("cargo", "-V"),
                "hashes": {str(path): sha(path) for path in [args.binary, args.worker, ROOT / "Cargo.lock", ROOT / "flake.lock", Path(__file__), ROOT / "packages/mermaid-render/examples/compare.rs", CORPUS / "manifest.json"]},
                "corpus": {name: {"sha256": sha(path), "bytes": path.stat().st_size} for name, path in sources.items()},
                "build": json.loads(args.build_record.read_text()),
                "build_record_sha256": sha(args.build_record),
                "deployment_artifacts": [{"path": str(path.resolve()), "bytes": path.stat().st_size, "sha256": sha(path)} for path in args.artifact],
                "omissions": ["No unique physical-memory peak; sampled tree RSS can miss short children and double count shared pages.",
                              "Per-request CPU unavailable: wait4 accounts entire trial host plus waited descendants on Linux.",
                              "Actual-stop proof unavailable from public render API; cancellation-return is NOT actual-stop.",
                              "Exact equality, dimensions/fonts do not establish raster/visual correctness.",
                              "Deployment artifact completeness and matched release/backend settings require coordinator review."]}
    (args.output / "metadata.json").write_text(json.dumps(metadata, indent=2))
    rng = random.Random(args.seed)
    schedule = []
    for index in range(args.trials):
        modes = list(args.modes)
        rng.shuffle(modes)
        schedule.extend((index, mode) for mode in modes if not (args.phase == "cancellation" and mode == "native"))
    (args.output / "schedule.json").write_text(json.dumps(schedule, indent=2))
    rows, trials, comparisons, batches = [], [], {}, []
    with (args.output / "raw.jsonl").open("w") as raw:
        for index, mode in schedule:
            names = list(manifest["diagnostic_native"] if mode == "native" else manifest["typical"] + manifest["stress"])
            if args.cases:
                names = [name for name in names if name in args.cases]
            # Native only bounded diagnostics, never bulk throughput/stress.
            rounds = 1 if mode == "native" or args.phase == "correctness" else args.rounds
            jobs = names * rounds
            rng.shuffle(jobs)
            replicas = []
            group_started = time.monotonic_ns()
            try:
                for _ in range(args.concurrency):
                    replicas.append(Trial(args.binary, args.worker, mode, args.timeout, args.sample_ms, args.fuel))
                with ThreadPoolExecutor(max_workers=args.concurrency) as pool:
                    for sequence, name in enumerate(jobs):
                        svg = args.output / f"{index}-{mode}-{sequence}-{name}.svg" if args.phase == "correctness" else "-"
                        # Each replica owns its process, protocol and wait4. All
                        # requests submitted before waiting; this tests concurrent
                        # hosts, not one engine's admission/async cancellation.
                        batch_started = time.monotonic_ns()
                        pending = [pool.submit(trial.render, sources[name],
                                               args.cancel_ms if args.phase == "cancellation" else "-", svg)
                                   for trial in replicas]
                        batch_rows = [future.result() for future in pending]
                        batches.append({"trial": index, "mode": mode, "case": name, "sequence": sequence,
                                        "concurrency": args.concurrency, "wall_ns": time.monotonic_ns() - batch_started,
                                        "successes": sum(row["status"] == "ok" for row in batch_rows)})
                        raw.write(json.dumps({"concurrent_batch": batches[-1]}) + "\n")
                        for replica, row in enumerate(batch_rows):
                            row.update({"trial": index, "mode": mode, "case": name, "sequence": sequence,
                                        "replica": replica, "concurrency": args.concurrency})
                            rows.append(row)
                            if args.phase == "correctness":
                                row["svg_info"] = svg_info(svg) if row["status"] == "ok" else None
                                comparisons.setdefault(name, []).append((mode, row, svg))
                            raw.write(json.dumps(row) + "\n")
                        raw.flush()
                        if mode == "native" and batch_rows[0]["status"] != "ok":
                            break
            finally:
                for replica, trial in enumerate(replicas):
                    record = trial.close()
                    record.update({"trial": index, "mode": mode, "replica": replica,
                                   "startup_roundtrip_ns": trial.startup_ns, "driver_setup_ns": int(trial.ready[1])})
                    trials.append(record)
                    raw.write(json.dumps({"trial_resources": record}) + "\n")
                raw.write(json.dumps({"concurrent_group": {"trial": index, "mode": mode,
                                     "concurrency": args.concurrency, "wall_ns": time.monotonic_ns() - group_started}}) + "\n")
    correctness = []
    for name, variants in comparisons.items():
        baseline = next((entry for entry in variants if entry[0] == "current"), None)
        if baseline is None:
            correctness.append({"case": name, "omission": "no current baseline"})
            continue
        _, base, base_path = baseline
        for mode, row, path in variants:
            match = base["svg_sha256"] == row["svg_sha256"] and base["status"] == row["status"]
            diff = None
            first_difference = None
            if not match and base["status"] == row["status"] == "ok":
                left, right = base_path.read_bytes(), path.read_bytes()
                offset = next((i for i, (a, b) in enumerate(zip(left, right)) if a != b), min(len(left), len(right)))
                first_difference = {"byte_offset": offset,
                                    "current": left[max(0, offset - 80):offset + 160].decode(errors="replace"),
                                    "candidate": right[max(0, offset - 80):offset + 160].decode(errors="replace")}
                diff = list(difflib.unified_diff(base_path.read_text().splitlines(), path.read_text().splitlines(),
                                               fromfile="current", tofile=mode))[:40]
                diff = [line[:500] for line in diff]
            correctness.append({"case": name, "mode": mode, "exact_match": match,
                                "baseline_status": base["status"], "status": row["status"],
                                "baseline_dimensions_fonts": base.get("svg_info"), "dimensions_fonts": row.get("svg_info"),
                                "bounded_diff": diff, "first_difference": first_difference})
    summary = []
    for mode in args.modes:
        for name in sources:
            group = [r for r in rows if r["mode"] == mode and r["case"] == name]
            valid = [r["render_ns"] for r in group if r["status"] == "ok"]
            cancel = [r["cancel_return_ns"] for r in group if r["status"] == "Cancelled" and r["cancel_return_ns"] is not None]
            if group:
                summary.append({"mode": mode, "case": name, "samples": len(group), "successes": len(valid),
                                "median_ns": quantile(valid, .5), "p95_ns": quantile(valid, .95),
                                "constructor_median_ns": quantile([r["constructor_ns"] for r in group if r["status"] == "ok"], .5),
                                "constructor_p95_ns": quantile([r["constructor_ns"] for r in group if r["status"] == "ok"], .95),
                                "render_call_median_ns": quantile([r["render_call_ns"] for r in group if r["status"] == "ok"], .5),
                                "render_call_p95_ns": quantile([r["render_call_ns"] for r in group if r["status"] == "ok"], .95),
                                "successful_render_throughput_per_second": len(valid) * 1e9 / sum(valid) if valid else None,
                                "cancel_return_p95_ns": quantile(cancel, .95),
                                "statuses": sorted({r["status"] for r in group})})
    groups = []
    for mode in args.modes:
        records = [t for t in trials if t["mode"] == mode]
        successful = sum(r["status"] == "ok" for r in rows if r["mode"] == mode)
        wall = sum(t["wall_ns"] for t in records)
        mode_batches = [batch for batch in batches if batch["mode"] == mode]
        batch_wall = sum(batch["wall_ns"] for batch in mode_batches)
        groups.append({"mode": mode, "concurrent_batch_success_throughput_per_second": sum(batch["successes"] for batch in mode_batches) * 1e9 / batch_wall if batch_wall else None,
                       "successful_trial_throughput_per_second": successful * 1e9 / wall if wall else None,
                       "trial_cpu_median_seconds": quantile([t["host_and_waited_child_cpu_seconds"] for t in records], .5),
                       "trial_cpu_p95_seconds": quantile([t["host_and_waited_child_cpu_seconds"] for t in records], .95),
                       "startup_median_ns": quantile([t["startup_roundtrip_ns"] for t in records], .5),
                       "startup_p95_ns": quantile([t["startup_roundtrip_ns"] for t in records], .95)})
    (args.output / "report.json").write_text(json.dumps({"summary": summary, "correctness": correctness,
                                                        "mode_resources": groups, "batches": batches, "trials": trials, "gates": "not evaluated; coordinator review required"}, indent=2))


if __name__ == "__main__":
    main()
