#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: scripts/repro-command-palette-flicker.sh [options]

Runs the production real-attach renderer with continuously changing pane output,
opens and exercises the command palette through its configured keybinding, and
captures render/performance artifacts for flicker analysis.

Options:
  --config FILE       bmux config copied into the sandbox
                      (default: $HOME/.config/nix/configs/bmux/bmux.toml)
  --output-dir DIR    artifact directory (default: target/palette-flicker-repro/<timestamp>)
  --viewport COLSxROWS
                      playbook viewport (default: 120x36)
  --open-key CHORD    command-palette attach chord (default: ctrl+shift+p)
  --skip-build        use the existing target/debug/bmux binary
  -h, --help          show this help
EOF
}

CONFIG_FILE="${HOME}/.config/nix/configs/bmux/bmux.toml"
OUTPUT_DIR=""
VIEWPORT="120x36"
OPEN_KEY="ctrl+shift+p"
SKIP_BUILD=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    --config) CONFIG_FILE="${2:?missing --config value}"; shift 2 ;;
    --output-dir) OUTPUT_DIR="${2:?missing --output-dir value}"; shift 2 ;;
    --viewport) VIEWPORT="${2:?missing --viewport value}"; shift 2 ;;
    --open-key) OPEN_KEY="${2:?missing --open-key value}"; shift 2 ;;
    --skip-build) SKIP_BUILD=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "error: unknown argument: $1" >&2; usage >&2; exit 2 ;;
  esac
done

[[ -f "$CONFIG_FILE" ]] || { echo "error: config file not found: $CONFIG_FILE" >&2; exit 2; }
case "$VIEWPORT" in *x*) ;; *) echo "error: viewport must be COLSxROWS" >&2; exit 2 ;; esac
COLS="${VIEWPORT%x*}"
ROWS="${VIEWPORT#*x}"
[[ "$COLS" =~ ^[0-9]+$ && "$ROWS" =~ ^[0-9]+$ ]] || { echo "error: invalid viewport: $VIEWPORT" >&2; exit 2; }

if [[ -z "$OUTPUT_DIR" ]]; then
  OUTPUT_DIR="target/palette-flicker-repro/$(date -u +%Y%m%dT%H%M%SZ)"
fi
mkdir -p "$OUTPUT_DIR"

BMUX_BIN="target/debug/bmux"
if [[ "$SKIP_BUILD" -eq 0 ]]; then
  cargo build -p bmux_cli
fi
[[ -x "$BMUX_BIN" ]] || { echo "error: bmux binary not found: $BMUX_BIN" >&2; exit 2; }

CONFIG_COPY="$OUTPUT_DIR/config-used.toml"
PLAYBOOK="$OUTPUT_DIR/playbook.dsl"
RESULT_JSON="$OUTPUT_DIR/playbook.json"
RUN_LOG="$OUTPUT_DIR/run.log"
PERF_JSON="$OUTPUT_DIR/perf.json"
RENDER_TRACE_JSON="$OUTPUT_DIR/render-trace.json"
METADATA_JSON="$OUTPUT_DIR/metadata.json"
cp "$CONFIG_FILE" "$CONFIG_COPY"
python3 - "$CONFIG_COPY" <<'PY'
import sys
path = sys.argv[1]
lines = open(path, encoding='utf-8').read().splitlines()
in_gateway = False
found_enabled = False
for index, line in enumerate(lines):
    stripped = line.strip()
    if stripped.startswith('[') and stripped.endswith(']'):
        in_gateway = stripped == '[server.gateway]'
        continue
    if in_gateway and stripped.startswith('enabled'):
        lines[index] = 'enabled = false'
        found_enabled = True
        break
if not found_enabled:
    lines.extend(['', '[server.gateway]', 'enabled = false'])
open(path, 'w', encoding='utf-8').write('\n'.join(lines) + '\n')
PY

cat > "$PLAYBOOK" <<EOF
@name command-palette-flicker-real-attach
@description Production command-palette flicker and over-render reproduction.
@driver real-attach
@viewport cols=$COLS rows=$ROWS
@timeout 45000
@record true
@render-trace true
@sandbox-config $CONFIG_COPY

new-session name=palette-flicker-repro
sleep ms=500
send-keys pane=1 keys='i=0; while [ \$i -lt 160 ]; do printf "palette-bg-%04d\\n" "\$i"; i=\$((i+1)); sleep 0.02; done\r'
sleep ms=250
render-mark id='before-palette'
send-attach key='$OPEN_KEY'
sleep ms=250
send-attach key='s'
send-attach key='e'
send-attach key='s'
send-attach key='s'
send-attach key='down'
send-attach key='down'
send-attach key='up'
resize-viewport cols=$((COLS - 8)) rows=$((ROWS - 2))
sleep ms=250
resize-viewport cols=$COLS rows=$ROWS
sleep ms=500
snapshot id=palette-open
send-attach key='esc'
sleep ms=300
snapshot id=palette-closed
assert-render since='before-palette' max_full_frame_frames=4 max_full_surface_fallbacks=160
EOF

set +e
BMUX_PLAYBOOK_HEADLESS_IMAGE_PROTOCOL=none "$BMUX_BIN" playbook run "$PLAYBOOK" \
  --json --record --viewport "$VIEWPORT" --timeout 45 --verbose \
  >"$RESULT_JSON" 2>"$RUN_LOG"
status=$?
set -e

recording_id="$(python3 - "$RESULT_JSON" <<'PY'
import json, sys
text = open(sys.argv[1], encoding='utf-8').read()
start = text.find('{')
if start < 0:
    print('')
else:
    print(json.loads(text[start:]).get('recording_id') or '')
PY
)"
if [[ -n "$recording_id" ]]; then
  "$BMUX_BIN" recording analyze "$recording_id" --perf --json >"$PERF_JSON" 2>>"$RUN_LOG" || printf '{}\n' >"$PERF_JSON"
else
  printf '{}\n' >"$PERF_JSON"
fi

python3 - "$RESULT_JSON" "$RENDER_TRACE_JSON" "$METADATA_JSON" "$RUN_LOG" "$status" "$VIEWPORT" <<'PY'
import json, re, sys
result_path, trace_path, metadata_path, run_log_path, status, viewport = sys.argv[1:]
text = open(result_path, encoding='utf-8').read()
start = text.find('{')
data = json.loads(text[start:]) if start >= 0 else {}
trace = [{
    'index': step.get('index'),
    'action': step.get('action'),
    'status': step.get('status'),
    'elapsed_ms': step.get('elapsed_ms'),
    'render_summary': step.get('render_summary'),
} for step in data.get('steps', []) if step.get('render_summary') is not None]
json.dump(trace, open(trace_path, 'w', encoding='utf-8'), indent=2)
log_text = open(run_log_path, encoding='utf-8').read()
metric_rows = []
for line in log_text.splitlines():
    if 'attach.metrics.window' not in line:
        continue
    fields = dict(re.findall(r'(\w+)=([^ ]+)', line))
    metric_rows.append({key: int(value) for key, value in fields.items() if value.isdigit()})
json.dump({
    'exit_status': int(status),
    'viewport': viewport,
    'recording_id': data.get('recording_id'),
    'result': result_path,
    'render_trace': trace_path,
    'attach_metric_windows': metric_rows,
    'attach_metric_totals': {
        key: sum(row.get(key, 0) for row in metric_rows)
        for key in sorted({key for row in metric_rows for key in row})
    },
}, open(metadata_path, 'w', encoding='utf-8'), indent=2)
PY

printf 'artifacts: %s\n' "$OUTPUT_DIR"
exit "$status"
