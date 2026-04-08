#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

KEEP_SMOKE_STATE="${KEEP_SMOKE_STATE:-0}"
SMOKE_TIMEOUT_SECS="${SMOKE_TIMEOUT_SECS:-60}"
SMOKE_MAX_RETRIES="${SMOKE_MAX_RETRIES:-2}"
SMOKE_RETRY_DELAY_SECS="${SMOKE_RETRY_DELAY_SECS:-1}"
declare -a SMOKE_SANDBOXES=()

create_sandbox() {
  local root
  root="$(mktemp -d "${TMPDIR:-/tmp}/bmux-smoke.XXXXXX")"
  mkdir -p "$root/config" "$root/data" "$root/runtime" "$root/state" "$root/logs" "$root/tmp"
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

run_with_timeout() {
  local timeout_secs="$1"
  shift

  "$@" &
  local cmd_pid=$!

  (
    sleep "$timeout_secs"
    kill -TERM "$cmd_pid" >/dev/null 2>&1 || true
    sleep 1
    kill -KILL "$cmd_pid" >/dev/null 2>&1 || true
  ) &
  local watchdog_pid=$!

  wait "$cmd_pid"
  local status=$?
  kill "$watchdog_pid" >/dev/null 2>&1 || true
  wait "$watchdog_pid" 2>/dev/null || true
  return "$status"
}

if ! command -v script >/dev/null 2>&1; then
  echo "skip: 'script' command not found"
  exit 0
fi

run_smoke_with_retry() {
  local label="$1"
  local shell_bin="$2"
  local payload="$3"
  local non_timeout_failure_message="$4"
  local sandbox
  local status
  local attempt=1
  local max_attempts=$((SMOKE_MAX_RETRIES + 1))

  while (( attempt <= max_attempts )); do
    sandbox="$(create_sandbox)"
    set +e
    run_with_timeout "$SMOKE_TIMEOUT_SECS" bash -lc "
      set -euo pipefail
      (
${payload}
      ) | XDG_CONFIG_HOME=\"$sandbox/config\" XDG_DATA_HOME=\"$sandbox/data\" XDG_RUNTIME_DIR=\"$sandbox/runtime\" BMUX_STATE_DIR=\"$sandbox/state\" BMUX_LOG_DIR=\"$sandbox/logs\" TMPDIR=\"$sandbox/tmp\" SHELL=\"$shell_bin\" script -q /dev/null cargo run -q -p bmux_cli -- >/dev/null 2>&1
    "
    status=$?
    set -e

    if [[ $status -eq 0 ]]; then
      echo "ok: ${label}"
      return 0
    fi

    if [[ $status -ne 143 ]]; then
      echo "fail: ${non_timeout_failure_message} ${status}"
      return 1
    fi

    if (( attempt == max_attempts )); then
      echo "fail: ${label} smoke failed after ${attempt} attempts (status ${status})"
      return 1
    fi

    echo "warn: ${label} smoke timed out (${status}), retrying ${attempt}/${SMOKE_MAX_RETRIES} after ${SMOKE_RETRY_DELAY_SECS}s"
    sleep "$SMOKE_RETRY_DELAY_SECS"
    attempt=$((attempt + 1))
  done

  return 1
}

run_case() {
  local shell_bin="$1"
  local shell_name
  shell_name="$(basename "$shell_bin")"

  if [[ ! -x "$shell_bin" ]]; then
    echo "skip: ${shell_name} not installed at ${shell_bin}"
    return 0
  fi

  run_smoke_with_retry \
    "$shell_name startup" \
    "$shell_bin" \
    "        sleep 1
        printf '\\001d'" \
    "${shell_name} startup smoke failed (status"
}

run_keybind_case() {
  local shell_bin="$1"

  if [[ ! -x "$shell_bin" ]]; then
    echo "skip: keybind smoke shell missing at ${shell_bin}"
    return 0
  fi

  run_smoke_with_retry \
    "keybind" \
    "$shell_bin" \
    "        sleep 1
        printf '\\001t'
        sleep 0.2
        printf '\\001d'" \
    "keybind smoke expected clean attach detach, got"
}

cd "$ROOT_DIR"
run_case /bin/sh
run_case /bin/bash
run_case /run/current-system/sw/bin/fish
run_case /bin/zsh
run_keybind_case /bin/sh

echo "smoke runtime checks passed"
