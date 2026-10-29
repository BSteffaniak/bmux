#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
KEEP_SMOKE_STATE="${KEEP_SMOKE_STATE:-${BMUX_KEEP_SMOKE_TMP:-0}}"
SMOKE_TIMEOUT_SECS="${SMOKE_TIMEOUT_SECS:-60}"
SMOKE_MAX_RETRIES="${SMOKE_MAX_RETRIES:-2}"
SMOKE_RETRY_DELAY_SECS="${SMOKE_RETRY_DELAY_SECS:-1}"
declare -a SMOKE_SANDBOXES=()

# Do not inherit a live runtime, attachment, target, or user configuration.
while IFS= read -r key; do
  unset "$key"
done < <(python3 -c 'import os; print("\n".join(k for k in os.environ if k.startswith("BMUX_")))')

sandbox_command() {
  local root="$1"
  shift
  HOME="$root/home" XDG_CONFIG_HOME="$root/config" XDG_DATA_HOME="$root/data" \
    XDG_STATE_HOME="$root/state" XDG_RUNTIME_DIR="$root/runtime" \
    BMUX_NO_BASE_CONFIG=1 BMUX_CONFIG_DIR="$root/config" BMUX_DATA_DIR="$root/data" \
    BMUX_RUNTIME_DIR="$root/runtime" BMUX_STATE_DIR="$root/state" \
    BMUX_LOG_DIR="$root/logs" TMPDIR="$root/tmp" "$@"
}

cleanup_sandboxes() {
  local sandbox
  for sandbox in "${SMOKE_SANDBOXES[@]}"; do
    # Stop only the runtime created in this explicit sandbox, never ambient state.
    sandbox_command "$sandbox" python3 -c '
import subprocess, sys
try:
    subprocess.run(sys.argv[1:], timeout=10, check=False, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
except subprocess.TimeoutExpired:
    print("smoke sandbox server stop timed out", file=sys.stderr)
' "$BMUX_SMOKE_BINARY" --runtime default --target local --config "$sandbox/smoke.toml" server stop
    if [[ "$KEEP_SMOKE_STATE" == "1" ]]; then
      echo "keep: smoke state at ${sandbox}"
    else
      rm -rf "$sandbox"
    fi
  done
}
trap cleanup_sandboxes EXIT

run_case() {
  local shell_bin="$1"
  local keybind="${2:-0}"
  local label
  label="$(basename "$shell_bin") startup"
  local -a driver_options=()
  if [[ "$keybind" == "1" ]]; then
    label="keybind"
    driver_options+=(--keybind)
  fi
  if [[ ! -x "$shell_bin" ]]; then
    echo "skip: ${shell_bin} not installed"
    return 0
  fi
  local attempt sandbox status
  for (( attempt=0; attempt<=SMOKE_MAX_RETRIES; attempt++ )); do
    sandbox="$(mktemp -d "${TMPDIR:-/tmp}/bmux-smoke.XXXXXX")"
    SMOKE_SANDBOXES+=("$sandbox")
    mkdir -p -m 700 "$sandbox"/{home,config,data,runtime,state,logs,tmp}
    printf '[keybindings]\ninitial_mode = "normal"\n' >"$sandbox/smoke.toml"
    status=0
    SHELL="$shell_bin" sandbox_command "$sandbox" python3 "$ROOT_DIR/scripts/smoke-pty.py" \
      --transcript "$sandbox/pty.log" --timeout "$SMOKE_TIMEOUT_SECS" "${driver_options[@]}" \
      -- "$BMUX_SMOKE_BINARY" --runtime default --target local --config "$sandbox/smoke.toml" || status=$?
    if [[ "$status" == "0" ]]; then
      echo "ok: ${label}"
      return 0
    fi
    if [[ "$status" != "124" ]] || (( attempt == SMOKE_MAX_RETRIES )); then
      echo "fail: ${label} smoke failed (status ${status})"
      tail -c 8192 "$sandbox/pty.log"
      return 1
    fi
    echo "warn: ${label} smoke deadline exceeded, retrying"
    sleep "$SMOKE_RETRY_DELAY_SECS"
  done
}

cd "$ROOT_DIR"
# Static plugins resolve declared auxiliary executables beside the host binary.
cargo build -q --locked -p bmux_cli -p bmux_pane_runtime_plugin --bins
BMUX_SMOKE_BINARY="${CARGO_TARGET_DIR:-$ROOT_DIR/target}/debug/bmux"
run_case /bin/sh
run_case /bin/bash
run_case /run/current-system/sw/bin/fish
run_case /bin/zsh
run_case /bin/sh 1
