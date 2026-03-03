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

  set +e
  script -q /dev/null \
    sh -lc "printf '\\001q' | cargo run -q -p bmux_cli -- --shell '$shell_bin' --no-alt-screen" \
    >/dev/null 2>&1
  local status=$?
  set -e

  if [[ $status -ne 0 ]]; then
    echo "fail: ${shell_name} startup smoke failed (status ${status})"
    return 1
  fi

  echo "ok: ${shell_name}"
}

run_keybind_case() {
  local shell_bin="$1"

  if [[ ! -x "$shell_bin" ]]; then
    echo "skip: keybind smoke shell missing at ${shell_bin}"
    return 0
  fi

  set +e
  script -q /dev/null \
    sh -lc "printf '\\001o\\001+\\001-\\001q' | cargo run -q -p bmux_cli -- --shell '$shell_bin' --no-alt-screen" \
    >/dev/null 2>&1
  local status=$?
  set -e

  if [[ $status -ne 0 ]]; then
    echo "fail: keybind smoke expected clean Ctrl-A q exit, got ${status}"
    return 1
  fi

  echo "ok: keybinds"
}

cd "$ROOT_DIR"
run_case /bin/sh
run_case /bin/bash
run_case /run/current-system/sw/bin/fish
run_case /bin/zsh
run_keybind_case /bin/sh

echo "smoke runtime checks passed"
