#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

KEEP_SMOKE_STATE="${KEEP_SMOKE_STATE:-0}"
declare -a SMOKE_SANDBOXES=()

create_sandbox() {
  local root
  root="$(mktemp -d "${TMPDIR:-/tmp}/bmux-smoke.XXXXXX")"
  mkdir -p "$root/config" "$root/data" "$root/runtime" "$root/tmp"
  SMOKE_SANDBOXES+=("$root")
  printf '%s' "$root"
}

cleanup_sandboxes() {
  local sandbox
  for sandbox in "${SMOKE_SANDBOXES[@]}"; do
    if [[ "$KEEP_SMOKE_STATE" == "1" ]]; then
      echo "keep: smoke state at ${sandbox}"
    else
      rm -rf "$sandbox"
    fi
  done
}

trap cleanup_sandboxes EXIT

if ! command -v script >/dev/null 2>&1; then
  echo "skip: 'script' command not found"
  exit 0
fi

run_case() {
  local shell_bin="$1"
  local shell_name
  local sandbox
  shell_name="$(basename "$shell_bin")"

  if [[ ! -x "$shell_bin" ]]; then
    echo "skip: ${shell_name} not installed at ${shell_bin}"
    return 0
  fi

  sandbox="$(create_sandbox)"
  set +e
  XDG_CONFIG_HOME="$sandbox/config" \
    XDG_DATA_HOME="$sandbox/data" \
    XDG_RUNTIME_DIR="$sandbox/runtime" \
    TMPDIR="$sandbox/tmp" \
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
  local sandbox

  if [[ ! -x "$shell_bin" ]]; then
    echo "skip: keybind smoke shell missing at ${shell_bin}"
    return 0
  fi

  sandbox="$(create_sandbox)"
  set +e
  XDG_CONFIG_HOME="$sandbox/config" \
    XDG_DATA_HOME="$sandbox/data" \
    XDG_RUNTIME_DIR="$sandbox/runtime" \
    TMPDIR="$sandbox/tmp" \
    script -q /dev/null \
    sh -lc "printf '\\001t\\001x\\001r\\001o\\001+\\001-\\001?\\001q' | cargo run -q -p bmux_cli -- --shell '$shell_bin' --no-alt-screen" \
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
