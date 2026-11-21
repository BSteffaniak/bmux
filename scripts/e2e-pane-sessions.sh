#!/usr/bin/env bash
# End-to-end check of in-server pane process management (docs/pane-execution.md):
# one process per pane, session sweeps on close and natural exit, crash records
# swept by the next server start, and `sweep_on_exit = false`.
# Usage: scripts/e2e-pane-sessions.sh /absolute/path/to/bmux   (KEEP=1 keeps the sandbox)
set -uo pipefail
B="$1"
while IFS= read -r key; do unset "$key"; done < <(python3 -c 'import os; print("\n".join(k for k in os.environ if k.startswith("BMUX_")))')
ROOT="$(mktemp -d "${TMPDIR:-/tmp}/bmux-e2e.XXXXXX")"
mkdir -p -m 700 "$ROOT"/{home,config,data,runtime,state,logs,tmp,marks}
fail=0
check() { if eval "$2"; then echo "ok:   $1"; else echo "FAIL: $1"; fail=1; fi; }
sb() {
  HOME="$ROOT/home" XDG_CONFIG_HOME="$ROOT/config" XDG_DATA_HOME="$ROOT/data" \
  XDG_STATE_HOME="$ROOT/state" XDG_RUNTIME_DIR="$ROOT/runtime" \
  BMUX_NO_BASE_CONFIG=1 BMUX_CONFIG_DIR="$ROOT/config" BMUX_DATA_DIR="$ROOT/data" \
  BMUX_RUNTIME_DIR="$ROOT/runtime" BMUX_STATE_DIR="$ROOT/state" BMUX_LOG_DIR="$ROOT/logs" \
  TMPDIR="$ROOT/tmp" SHELL="$ROOT/shell.sh" "$@"
}
# The pane "shell": start a HUP/TERM-ignoring background job in its own group,
# record its pid, then either stay (default) or exit (EXIT_NOW set).
cat >"$ROOT/shell.sh" <<EOF
#!/bin/sh
set -m
(trap '' HUP TERM; exec sleep 600) &
echo \$! > "$ROOT/marks/job.\$\$"
[ -f "$ROOT/marks/exit-now" ] && exit 0
exec sleep 600
EOF
chmod +x "$ROOT/shell.sh"
CONF="$ROOT/config/e2e.toml"
printf '[keybindings]\ninitial_mode = "normal"\n' >"$CONF"
alive() { kill -0 "$1" 2>/dev/null && [ "$(ps -o stat= -p "$1" | cut -c1)" != Z ]; }
jobs_now() { cat "$ROOT"/marks/job.* 2>/dev/null; }
server_pid() { pgrep -f "^$B .*server start" | head -1; }
start_server() { sb "$B" --config "$CONF" server start --daemon >/dev/null 2>&1; sleep 2; }
new_session() { sb "$B" --config "$CONF" new-session "$1" >/dev/null 2>&1; sleep 2; }
cleanup() { sb "$B" --config "$CONF" server stop >/dev/null 2>&1; for p in $(jobs_now); do kill -9 "$p" 2>/dev/null; done; [ "${KEEP:-0}" = 1 ] && echo "kept $ROOT" || rm -rf "$ROOT"; }
trap cleanup EXIT

start_server
S=$(server_pid)
check "server running" '[ -n "$S" ]'
new_session one; new_session two; new_session three
check "three panes recorded jobs" '[ "$(jobs_now | wc -l | tr -d " ")" = 3 ]'
children=$(pgrep -P "$S" | wc -l | tr -d ' ')
helpers=$(for c in $(pgrep -P "$S"); do ps -o command= -p "$c"; done | grep -vc "$ROOT/shell.sh\|sleep" )
echo "info: server children=$children supervisor-processes=$helpers"
check "server children are only the three pane shells" '[ "$children" = 3 ] && [ "$helpers" = 0 ]'
check "pane session records exist" '[ "$(ls "$ROOT"/runtime/*/pane-sessions/*.json 2>/dev/null | wc -l | tr -d " ")" -ge 3 ] || [ "$(find "$ROOT/runtime" -path "*pane-sessions/*.json" | wc -l | tr -d " ")" -ge 3 ]'

# A: closing a session kills its HUP-ignoring job.
job_one=$(cat "$ROOT"/marks/job.* | head -1)
sb "$B" --config "$CONF" kill-all-sessions >/dev/null 2>&1; sleep 2
dead_all=1; for p in $(jobs_now); do alive "$p" && dead_all=0; done
check "kill-all-sessions swept HUP-ignoring jobs" '[ $dead_all = 1 ]'
rm -f "$ROOT"/marks/job.*

# B: server SIGKILL leaves HUP-ignoring jobs; next start sweeps them.
new_session crash
crash_job=$(jobs_now | head -1)
kill -9 "$(server_pid)"; sleep 2
check "HUP-ignoring job survives server SIGKILL" 'alive "$crash_job"'
check "crash record left behind" '[ "$(find "$ROOT/runtime" -path "*pane-sessions/*.json" | wc -l | tr -d " ")" -ge 1 ]'
start_server
sleep 1
check "next server start swept the crashed session" '! alive "$crash_job"'
check "crash record removed" '[ "$(find "$ROOT/runtime" -path "*pane-sessions/*.json" | wc -l | tr -d " ")" = 0 ] || [ "$(sb "$B" --config "$CONF" list-sessions 2>/dev/null | grep -c crash)" -ge 1 ]'
sb "$B" --config "$CONF" kill-all-sessions >/dev/null 2>&1; sleep 1
rm -f "$ROOT"/marks/job.*

# C: natural shell exit sweeps leftover jobs by default.
touch "$ROOT/marks/exit-now"
new_session exits; sleep 2
exit_job=$(jobs_now | head -1)
check "natural exit swept the background job" '[ -n "$exit_job" ] && ! alive "$exit_job"'
rm -f "$ROOT"/marks/job.*
sb "$B" --config "$CONF" kill-all-sessions >/dev/null 2>&1; sleep 1
sb "$B" --config "$CONF" server stop >/dev/null 2>&1; sleep 2

# D: sweep_on_exit = false leaves it running.
printf '[keybindings]\ninitial_mode = "normal"\n[plugins.settings."bmux.pane_runtime".execution]\nsweep_on_exit = false\n' >"$CONF"
start_server
new_session keeps; sleep 2
keep_job=$(jobs_now | head -1)
tracked=$(find "$ROOT/runtime" -path "*pane-sessions/*.json" | wc -l | tr -d " ")
check "sweep_on_exit=false leaves the job running" '[ -n "$keep_job" ] && alive "$keep_job"'
sb "$B" --config "$CONF" kill-all-sessions >/dev/null 2>&1; sleep 2
if [ "$tracked" -ge 1 ]; then
  check "closing the pane sweeps a survivor still tracked" '! alive "$keep_job"'
else
  check "a survivor released at terminal EOF keeps running after close" 'alive "$keep_job"'
fi
exit $fail
