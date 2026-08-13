#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${root}"

forbidden='bcode|filesystem|request.?draft|provider|tool.?call|plugin|session.?view'
output="$(rg -n -i "${forbidden}" packages/tui-runtime \
    --glob '*.rs' --glob 'Cargo.toml' --glob '*.md' || true)"
if [[ -n "${output}" ]]; then
    echo "tui-runtime domain-neutrality violation: generic runtime sources contain product-domain terminology" >&2
    printf '%s\n' "${output}" >&2
    exit 1
fi

echo "tui-runtime domain-neutrality guard passed"
