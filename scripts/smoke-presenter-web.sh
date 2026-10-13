#!/usr/bin/env bash
# No builds, installs, or implicit artifact discovery. Run only under coordinator lock.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
exec python3 -B "$ROOT/scripts/presenter-web-acceptance.py" "$@"
