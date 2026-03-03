#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

if ! command -v script >/dev/null 2>&1; then
  echo "skip: 'script' command not found"
  exit 0
fi

run_case() {
  local shell_bin="$1"
  local shell_name
  shell_name="$(basename "$shell_bin")"

  if [[ ! -x "$shell_bin" ]]; then
    echo "skip: ${shell_name} not installed at ${shell_bin}"
    return 0
  fi

  local output_file
  output_file="$(mktemp)"

  set +e
  script -q /dev/null \
    sh -lc "printf 'echo BMUX_SMOKE_OK\\nexit\\n' | cargo run -q -p bmux_cli -- --shell '$shell_bin' --no-alt-screen" \
    >"$output_file" 2>&1
  local status=$?
  set -e

  if [[ $status -ne 0 ]]; then
    echo "fail: ${shell_name} startup smoke failed (status ${status})"
    cat "$output_file"
    rm -f "$output_file"
    return 1
  fi

  if ! rg -q "BMUX_SMOKE_OK" "$output_file"; then
    echo "fail: ${shell_name} output did not include BMUX_SMOKE_OK"
    cat "$output_file"
    rm -f "$output_file"
    return 1
  fi

  set +e
  script -q /dev/null \
    sh -lc "printf 'exit 7\\n' | cargo run -q -p bmux_cli -- --shell '$shell_bin' --no-alt-screen" \
    >/dev/null 2>&1
  status=$?
  set -e

  if [[ $status -ne 7 ]]; then
    echo "fail: ${shell_name} exit code propagation expected 7, got ${status}"
    rm -f "$output_file"
    return 1
  fi

  echo "ok: ${shell_name}"
  rm -f "$output_file"
}

cd "$ROOT_DIR"
run_case /bin/sh
run_case /bin/bash
run_case /run/current-system/sw/bin/fish
run_case /bin/zsh

echo "smoke runtime checks passed"
