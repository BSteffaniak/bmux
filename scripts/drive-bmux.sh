#!/usr/bin/env bash
# Drive this checkout's bmux UI with `bmux drive`, isolated from your own bmux:
# its own config, runtime, data, state and log directories, and its own server.
#
#   scripts/drive-bmux.sh start [--size 120x40] [--config FILE] [-- BMUX_ARGS...]
#   bmux drive -s bmux-ui press ...      # then drive it like any program
#   scripts/drive-bmux.sh stop           # stops the session and its bmux server
#
# BMUX_BIN selects the binary (default: the debug build); DRIVE_BMUX_NAME the
# session name (default: bmux-ui).
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BMUX="${BMUX_BIN:-${CARGO_TARGET_DIR:-$ROOT_DIR/target}/debug/bmux}"
NAME="${DRIVE_BMUX_NAME:-bmux-ui}"
STATE="${TMPDIR:-/tmp}/drive-bmux-$(id -u)-$NAME"

inner_env=(
  "BMUX_NO_BASE_CONFIG=1"
  "BMUX_CONFIG_DIR=$STATE/config"
  "BMUX_DATA_DIR=$STATE/data"
  "BMUX_RUNTIME_DIR=$STATE/runtime"
  "BMUX_STATE_DIR=$STATE/state"
  "BMUX_LOG_DIR=$STATE/logs"
)

usage() {
  sed -n '2,10p' "$0" | sed 's/^# \{0,1\}//'
  exit 2
}

start() {
  local size="120x40" config=""
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --size) size="$2"; shift 2 ;;
      --config) config="$2"; shift 2 ;;
      --) shift; break ;;
      *) usage ;;
    esac
  done
  if [[ -e "$STATE" ]]; then
    echo "error: $STATE exists; run '$0 stop' first" >&2
    exit 1
  fi
  mkdir -p "$STATE"
  chmod 700 "$STATE"
  for dir in config data runtime state logs; do
    mkdir -m 700 "$STATE/$dir"
  done
  if [[ -n "$config" ]]; then
    cp "$config" "$STATE/config/bmux.toml"
  fi
  local env_args=()
  for entry in "${inner_env[@]}"; do
    env_args+=(--env "$entry")
  done
  "$BMUX" drive start --name "$NAME" --size "$size" --timeout 120 --no-screen "${env_args[@]}" \
    -- "$BMUX" --runtime default --target local "$@"
  # The client waits while its server starts; report the first real screen.
  "$BMUX" drive -s "$NAME" wait --gone 'server is still starting|started in daemon mode' --timeout 180
}

stop() {
  local status=0
  "$BMUX" drive -s "$NAME" stop --no-screen || status=$?
  env "${inner_env[@]}" "$BMUX" --runtime default --target local server stop >/dev/null 2>&1 || true
  rm -rf "$STATE"
  return "$status"
}

case "${1:-}" in
  start) shift; start "$@" ;;
  stop) stop ;;
  *) usage ;;
esac
