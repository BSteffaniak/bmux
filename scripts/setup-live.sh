#!/usr/bin/env bash
# One-command interactive setup; no shell-specific exports or manual Nix entry.
set +x
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
if [[ -z "${BMUX_NIX_TOOLCHAIN:-}" ]]; then
  exec bash scripts/dev.sh bash scripts/setup-live.sh "$@"
fi
if [[ "${1:-}" == --fly ]]; then
  shift
  exec python3 scripts/setup-live-fly.py "$@"
fi
exec python3 scripts/setup-live.py "$@"
