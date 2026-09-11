#!/usr/bin/env bash
# Stable entry point for agents, editors, and scripts without direnv integration.
set -euo pipefail
ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
if [[ $# -eq 0 ]]; then
  echo "Usage: bash scripts/dev.sh <command> [arguments...]" >&2
  exit 2
fi
exec nix develop "$ROOT_DIR" --command "$@"
