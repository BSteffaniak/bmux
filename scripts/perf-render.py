#!/usr/bin/env python3
"""Reproducible render-path metrics: check, update and A/B-compare them.

The client pipeline (`pipeline_bench::pipeline_metrics` in bmux_attach_pipeline) and the pane
reader (`runtime::ingest_bench::ingest_metrics` in bmux_pane_runtime_plugin) report, per frame and
phase, allocations, allocated bytes, bytes written and instructions retired. Allocation and byte
counts repeat exactly between runs, and instructions nearly so (each frame keeps its fewest across
repeated runs), so they are compared against committed baselines in
`docs/perf-baselines/render/<bench>-<os>.json` and any growth fails. `work` builds the benchmarks
with coverage instrumentation and counts the code each scenario executes: units of work that are
exact on any machine, gated against `work-<bench>-<os>.json`, with a per-function profile.
See docs/development/render-performance.md.

  scripts/perf-render.py check [--soak] [--work] [--out DIR]   run and gate against the baselines
  scripts/perf-render.py update [--work]                        rewrite this OS's baselines
  scripts/perf-render.py compare BASE [HEAD]                    run two revisions, print the change
  scripts/perf-render.py work [--scenario NAME] [--top N]       print where each scenario's work goes

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
# The test each benchmark runs and the library target its test binary is built from.
TESTS = {
    "pipeline": ("pipeline_bench::pipeline_metrics", "bmux_attach_pipeline"),
    "ingest": ("runtime::ingest_bench::ingest_metrics", "bmux_pane_runtime_plugin"),
}
# Frames a scenario runs before its measured ones (the benchmarks' warm-up).
WARMUP = 30

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


def work_baseline_path(bench: str) -> Path:
    return BASELINES / f"work-{bench}-{OS}.json"


def llvm_profdata() -> str:
    """llvm-profdata of the toolchain's LLVM: LLVM_PROFDATA, the toolchain's own llvm-tools
    component (rustup), or the flake's llvm-tools package."""
    if os.environ.get("LLVM_PROFDATA"):
        return os.environ["LLVM_PROFDATA"]
    sysroot = subprocess.run(["rustc", "--print", "sysroot"], capture_output=True, text=True,
                             check=True).stdout.strip()
    candidates = list(Path(sysroot).glob("lib/rustlib/*/bin/llvm-profdata"))
    if not candidates:
        tools = subprocess.run(["nix", "build", "--no-link", "--print-out-paths", ".#llvm-tools"],
                               cwd=ROOT, capture_output=True, text=True, check=True)
        candidates = list(Path(tools.stdout.strip()).glob("lib/rustlib/*/bin/llvm-profdata"))
    if not candidates:
        sys.exit("llvm-profdata not found: set LLVM_PROFDATA or add the llvm-tools component")
    return str(candidates[0])


def coverage_binary(bench: str) -> str:
    """Build `bench`'s test binary with coverage instrumentation, in its own target directory so
    ordinary builds stay as they are, and return its path."""
    _, target = TESTS[bench]
    args = BENCHES[bench]
    package_args = [arg for arg in args[:-1] if arg != "--lib"]
    env = {
        **os.environ,
        # Counters sit in the code before optimization, so units do not depend on it: a lightly
        # optimized dev build counts the same and compiles far faster than an instrumented release
        # build. Debug assertions stay off, as in release, so they add no counted code.
        "RUSTFLAGS": (os.environ.get("RUSTFLAGS", "")
                      + " -C instrument-coverage -C opt-level=1 -C debug-assertions=off").strip(),
        "CARGO_TARGET_DIR": os.environ.get("CARGO_TARGET_DIR", str(ROOT / "target")) + "-coverage",
        # Instrumented build scripts and proc macros write profiles too: keep them out of the tree.
        "LLVM_PROFILE_FILE": str(Path(tempfile.gettempdir()) / "perf-render-build-%p.profraw"),
    }
    build = subprocess.run(
        ["cargo", "test", "-j", os.environ.get("PERF_RENDER_JOBS", "4"), *package_args,
         "--lib", "--no-run", "--message-format=json"],
        cwd=ROOT, env=env, capture_output=True, text=True, check=False)
    for stale in Path(tempfile.gettempdir()).glob("perf-render-build-*.profraw"):
        stale.unlink(missing_ok=True)
    for line in build.stdout.splitlines():
        try:
            message = json.loads(line)
        except json.JSONDecodeError:
            continue
        if (message.get("reason") == "compiler-artifact" and message.get("executable")
                and message["target"]["name"] == target):
            return message["executable"]
    sys.exit(f"building {bench} with coverage failed:\n{build.stderr[-4000:]}")


def work_tool(*args: str) -> subprocess.CompletedProcess:
    return subprocess.run(
        ["cargo", "run", "-q", "--release", "-p", "bmux_bench", "--bin", "bmux-bench-work", "--",
         *args], cwd=ROOT, capture_output=True, text=True, check=False)


def run_work(bench: str, out: Path, only: str | None = None, top: int = 40) -> dict:
    """Profile each of `bench`'s scenarios (those of its metrics baseline) in its own process and
    return its work report."""
    test, _ = TESTS[bench]
    binary = coverage_binary(bench)
    profdata = llvm_profdata()
    scenarios = json.loads(baseline_path(bench).read_text())["scenarios"]
    report = {"scenarios": []}
    for scenario in scenarios:
        name = scenario["name"]
        if only and name != only:
            continue
        stem = out / f"{bench}-{name.replace('/', '_')}"
        for stale in out.glob(f"{stem.name}-*.profraw"):
            stale.unlink()
        env = {**os.environ, "BMUX_BENCH_ONLY": name, "BMUX_BENCH_REPEATS": "1",
               "LLVM_PROFILE_FILE": f"{stem}-%p.profraw"}
        env.pop("BMUX_BENCH_JSON", None)
        env.pop("BMUX_BENCH_BASELINE", None)
        run = subprocess.run([binary, "--ignored", "--exact", test, "--test-threads", "1"],
                             env=env, capture_output=True, text=True, check=False)
        if run.returncode != 0:
            sys.exit(f"{bench} {name} failed under coverage:\n{run.stdout[-2000:]}{run.stderr[-2000:]}")
        merged = stem.with_suffix(".profdata")
        subprocess.run([profdata, "merge", "-sparse", *map(str, out.glob(f"{stem.name}-*.profraw")),
                        "-o", str(merged)], check=True)
        show = stem.with_suffix(".txt")
        with show.open("w") as handle:
            subprocess.run([profdata, "show", "--all-functions", "--counts", str(merged)],
                           stdout=handle, check=True)
        frames = scenario["frames"] + WARMUP
        summary = work_tool("summarize", "--show", str(show), "--scenario", name, "--frames",
                            str(frames), "--top", str(top))
        if summary.returncode != 0:
            sys.exit(summary.stderr)
        report["scenarios"].append(json.loads(summary.stdout))
        print(f"{bench} {name}: {report['scenarios'][-1]['units_per_frame']} units/frame",
              flush=True)
    return report


def check_work(out: Path) -> list[str]:
    failed = []
    for bench in BENCHES:
        report_path = out / work_baseline_path(bench).name
        report_path.write_text(json.dumps(run_work(bench, out), indent=2) + "\n")
        baseline = work_baseline_path(bench)
        if not baseline.exists():
            print(f"warning: no {OS} work baseline for {bench}; wrote {report_path} (commit it as "
                  f"{baseline.relative_to(ROOT)} to gate)", flush=True)
            continue
        result = work_tool("check", "--report", str(report_path), "--baseline", str(baseline))
        print(result.stdout, end="", flush=True)
        if result.returncode != 0:
            failed.append(f"{bench} work")
    return failed


def work(args: argparse.Namespace) -> int:
    out = Path(tempfile.mkdtemp(prefix="perf-render-work-"))
    for bench in BENCHES:
        report = run_work(bench, out, args.scenario, args.top)
        for scenario in report["scenarios"]:
            print(f"{bench} {scenario['name']}, busiest functions per frame:")
            for function in scenario["functions"][: args.top]:
                print(f"  {function['units_per_frame']:>12.2f} units  "
                      f"{function['entries_per_frame']:>10.2f} calls  {function['name']}")
    print(f"profiles: {out}")
    return 0


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
    if args.work:
        failed.extend(check_work(out))
    print(f"reports: {out}")
    if failed:
        print(f"failed: {', '.join(failed)}", file=sys.stderr)
        return 1
    return 0


def update(args: argparse.Namespace) -> int:
    BASELINES.mkdir(parents=True, exist_ok=True)
    for bench in BENCHES:
        if not run_bench(bench, ROOT, baseline_path(bench)):
            return 1
        print(f"wrote {baseline_path(bench).relative_to(ROOT)}")
    if args.work:
        out = Path(tempfile.mkdtemp(prefix="perf-render-work-"))
        for bench in BENCHES:
            path = work_baseline_path(bench)
            path.write_text(json.dumps(run_work(bench, out), indent=2) + "\n")
            print(f"wrote {path.relative_to(ROOT)}")
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
    check_parser.add_argument("--work", action="store_true",
                              help="also gate units of work (coverage-instrumented builds)")
    check_parser.add_argument("--out", help="directory for the reports written")
    check_parser.set_defaults(run=check)
    update_parser = commands.add_parser("update", help="rewrite this OS's baselines")
    update_parser.add_argument("--work", action="store_true", help="also the work baselines")
    update_parser.set_defaults(run=update)
    work_parser = commands.add_parser("work", help="print where each scenario's work goes")
    work_parser.add_argument("--scenario", help="only this scenario")
    work_parser.add_argument("--top", type=int, default=15, help="functions per scenario")
    work_parser.set_defaults(run=work)
    compare_parser = commands.add_parser("compare", help="A/B two revisions")
    compare_parser.add_argument("base")
    compare_parser.add_argument("head", nargs="?", default="HEAD")
    compare_parser.set_defaults(run=compare)
    args = parser.parse_args()
    return args.run(args)


if __name__ == "__main__":
    sys.exit(main())
