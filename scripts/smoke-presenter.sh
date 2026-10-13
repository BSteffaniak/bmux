#!/usr/bin/env bash
# No build/install: coordinator supplies a compatible staged binary + plugin directory.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
exec python3 "$ROOT/plugins/presenter-plugin/tests/acceptance.py" "$@"
