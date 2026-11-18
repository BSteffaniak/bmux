# Grapheme release microbenchmark

Run from this directory in the repository Nix development shell; keep its
compiler and target directory. The two Cargo manifests are standalone
workspaces. The upstream executable has no candidate dependency. To print one
CSV run per implementation:

```sh
cargo run --locked --offline --release
cargo run --manifest-path candidate/Cargo.toml --locked --offline --release
```

`bench.rs` measures nanoseconds per complete operation, excluding corpus setup.
`fwd`/`rev` consume graphemes; `ends` alternates indexed front/back; `erase`
constructs a fresh indexed iterator and removes the last grapheme by shortening
a borrowed prefix until empty. This isolates boundary discovery, not full editor
input latency. Repetitions scale 256 → 512 → 1024; Indic, Extend and ZWJ are
single long clusters, RI a single contiguous run. Allocation counts cover one
complete operation outside timing, not process startup. Output digests check
comparable work, not Unicode conformance.

## Recorded comparison

2026-11-18, Apple M5 Pro, aarch64-apple-darwin, Nix rustc 1.99.1 (58cf8f9c3),
optimized defaults. Raw files are in `evidence/`. For each cell, three separate
processes each took five samples after calibrating batches to at least 2 ms.
Compare medians of process medians. The three upstream-only runs came first.
Before any candidate build, `frozen-budget.txt` recorded the baseline hashes,
report hash and margin: twice p95 run spread rounded up to a five-point step,
clamped to 10–20%. Excess noise cannot increase the allowed regression. Check
candidate time and doubling ratios against baseline × 1.20, require zero
allocations and matching digests. Run `python3 report.py candidate` for the
84-cell comparison. `candidate-source.sha256` identifies the measured source,
not a promise about later edits.

Baseline p95 spread was 353.39%, so the 20% cap applied. Primary comparison: 60
pass, 24 noisy; no median time or doubling ratio crossed budget. A second
sequence (old/new, new/old, old/new) in `evidence/paired/` again had no median
budget violations, but 36 noisy rows. `noisy` means at least one leg's three
process medians spanned >20% for that cell. Doubling estimates also inherit
noise from the smaller cell. The raw samples are descriptive, not confidence
intervals. No CPU affinity or exclusive machine. Repeated whole-prefix RI
erasure still takes approximately four times as long for twice the input on both
implementations. Do not infer a linear worst-case editor guarantee.
