#!/usr/bin/env bash
# CPU-only retained composition/direct acceptance scenarios. Does not claim GPU,
# display-server, end-to-end native latency, or manual UX verification.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
# Ordinary Cargo toolchain/target selection is owned by the Nix development shell.
# Optional first argument stores the complete Cargo output as a caller-owned artifact.
if [[ $# -gt 1 ]]; then
    echo "usage: bash perf/bterm-direct-presentation.sh [artifact-path]" >&2
    exit 2
fi
run() {
    bash scripts/dev.sh cargo test --locked -p bmux_cli --test bterm_presentation \
        bterm_direct_presentation_perf -- --ignored --nocapture
}
if [[ $# -eq 1 ]]; then
    run 2>&1 | tee "$1"
else
    run
fi
