#!/usr/bin/env python3
"""Reproducible render-path metrics: check, update and A/B-compare them.

The client pipeline (`pipeline_bench::pipeline_metrics` in bmux_attach_pipeline) and the pane
reader (`runtime::ingest_bench::ingest_metrics` in bmux_pane_runtime_plugin) report, per frame and
phase, allocations, allocated bytes, bytes written and instructions retired. Allocation and byte
counts repeat exactly between runs, so they are compared against committed baselines in
`docs/perf-baselines/render/<bench>-<os>.json` and any growth fails. Instructions are reported, never
gated (they differ between machines). See docs/development/render-performance.md.

  scripts/perf-render.py check [--soak] [--out DIR]   run and gate against the baselines
  scripts/perf-render.py update                        rewrite this OS's baselines
  scripts/perf-render.py compare BASE [HEAD]           run two revisions and print the difference

Run inside the Nix development shell (direnv), from the repository root.
"""

from __future__ import annotations

import argparse
import json
import os
import platform
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
BASELINES = ROOT / "docs" / "perf-baselines" / "render"
OS = {"Darwin": "macos", "Linux": "linux"}.get(platform.system(), platform.system().lower())

BENCHES = {
    "pipeline": [
        "-p", "bmux_attach_pipeline", "--features", "image-kitty,image-sixel", "--lib",
        "pipeline_bench::pipeline_metrics",
    ],
    "ingest": [
        "-p", "bmux_pane_runtime_plugin", "--features", "image-registry", "--lib",
        "runtime::ingest_bench::ingest_metrics",
    ],
}
SOAKS = {
    "pipeline": [
        "-p", "bmux_attach_pipeline", "--features", "image-kitty,image-sixel", "--lib",
        "pipeline_bench::pipeline_soak",
    ],
    "ingest": [
        "-p", "bmux_pane_runtime_plugin", "--features", "image-registry", "--lib",
        "runtime::ingest_bench::ingest_soak",
    ],
}


def baseline_path(bench: str) -> Path:
    return BASELINES / f"{bench}-{OS}.json"


def cargo_test(args: list[str], cwd: Path, env: dict[str, str]) -> subprocess.CompletedProcess:
    command = [
        "cargo", "test", "--release", "-j", os.environ.get("PERF_RENDER_JOBS", "4"), *args,
        "--", "--ignored", "--exact", "--nocapture", "--test-threads", "1",
    ]
    print("+", " ".join(command), flush=True)
    return subprocess.run(command, cwd=cwd, env={**os.environ, **env}, check=False)


def run_bench(bench: str, cwd: Path, out: Path, baseline: Path | None = None) -> bool:
    env = {"BMUX_BENCH_JSON": str(out)}
    if baseline is not None:
        env["BMUX_BENCH_BASELINE"] = str(baseline)
    return cargo_test(BENCHES[bench], cwd, env).returncode == 0


def check(args: argparse.Namespace) -> int:
    out = Path(args.out) if args.out else Path(tempfile.mkdtemp(prefix="perf-render-"))
    out.mkdir(parents=True, exist_ok=True)
    failed = []
    for bench in BENCHES:
        baseline = baseline_path(bench)
        report = out / baseline.name
        if not baseline.exists():
            print(f"warning: no {OS} baseline for {bench}; wrote {report} (commit it as "
                  f"{baseline.relative_to(ROOT)} to gate)", flush=True)
            if not run_bench(bench, ROOT, report):
                failed.append(bench)
            continue
        if not run_bench(bench, ROOT, report, baseline):
            failed.append(bench)
    if args.soak:
        for bench, test in SOAKS.items():
            if cargo_test(test, ROOT, {}).returncode != 0:
                failed.append(f"{bench} soak")
    print(f"reports: {out}")
    if failed:
        print(f"failed: {', '.join(failed)}", file=sys.stderr)
        return 1
    return 0


def update(_: argparse.Namespace) -> int:
    BASELINES.mkdir(parents=True, exist_ok=True)
    for bench in BENCHES:
        if not run_bench(bench, ROOT, baseline_path(bench)):
            return 1
        print(f"wrote {baseline_path(bench).relative_to(ROOT)}")
    return 0


def run_revision(revision: str, out: Path) -> dict[str, dict]:
    """Run every bench at `revision` (a temporary worktree), returning its reports."""
    reports = {}
    with tempfile.TemporaryDirectory(prefix="perf-render-") as scratch:
        tree = Path(scratch) / "tree"
        subprocess.run(["git", "worktree", "add", "--detach", "-q", str(tree), revision],
                       cwd=ROOT, check=True)
        try:
            for bench in BENCHES:
                report = out / f"{bench}-{revision.replace('/', '_')}.json"
                if run_bench(bench, tree, report) and report.exists():
                    reports[bench] = json.loads(report.read_text())
                else:
                    print(f"warning: {bench} did not run at {revision}", flush=True)
        finally:
            subprocess.run(["git", "worktree", "remove", "--force", str(tree)], cwd=ROOT,
                           check=False)
    return reports


def change(before: float | None, after: float | None) -> str:
    if before is None or after is None:
        return ""
    if before == 0:
        return "" if after == 0 else "new"
    return f"{100 * (after - before) / before:+.1f}%"


def compare(args: argparse.Namespace) -> int:
    out = Path(tempfile.mkdtemp(prefix="perf-render-compare-"))
    base = run_revision(args.base, out)
    head = run_revision(args.head, out)
    rows = [("bench", "scenario", "metric", args.base, args.head, "change")]
    for bench, after_report in head.items():
        before_report = base.get(bench, {"scenarios": []})
        before_scenarios = {s["name"]: s for s in before_report["scenarios"]}
        for scenario in after_report["scenarios"]:
            before = before_scenarios.get(scenario["name"])
            for phase, metrics in scenario["phases"].items():
                old = (before or {}).get("phases", {}).get(phase, {})
                for metric, label in (
                    ("allocations_per_frame", "allocs/frame"),
                    ("allocated_bytes_per_frame", "alloc B/frame"),
                    ("instructions_per_frame", "instr/frame"),
                ):
                    if metric in metrics:
                        rows.append((bench, scenario["name"], f"{phase} {label}",
                                     str(old.get(metric, "")), str(metrics[metric]),
                                     change(old.get(metric), metrics[metric])))
            if bench == "pipeline":
                rows.append((bench, scenario["name"], "output B/frame",
                             str((before or {}).get("output_bytes_per_frame", "")),
                             str(scenario["output_bytes_per_frame"]),
                             change((before or {}).get("output_bytes_per_frame"),
                                    scenario["output_bytes_per_frame"])))
    widths = [max(len(row[i]) for row in rows) for i in range(len(rows[0]))]
    for row in rows:
        print("  ".join(cell.ljust(width) for cell, width in zip(row, widths)))
    print(f"reports: {out}")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    commands = parser.add_subparsers(dest="command", required=True)
    check_parser = commands.add_parser("check", help="run and gate against the baselines")
    check_parser.add_argument("--soak", action="store_true", help="also run the memory soaks")
    check_parser.add_argument("--out", help="directory for the reports written")
    check_parser.set_defaults(run=check)
    commands.add_parser("update", help="rewrite this OS's baselines").set_defaults(run=update)
    compare_parser = commands.add_parser("compare", help="A/B two revisions")
    compare_parser.add_argument("base")
    compare_parser.add_argument("head", nargs="?", default="HEAD")
    compare_parser.set_defaults(run=compare)
    args = parser.parse_args()
    return args.run(args)


if __name__ == "__main__":
    sys.exit(main())
