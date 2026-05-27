#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: scripts/repro-decoration-flicker.sh [options]

Runs an isolated production real-attach playbook using the user's decoration
configuration, records the attach stream, and exports artifacts for flicker
analysis.

Options:
  --config FILE       bmux config file to copy into the sandbox
                      (default: $HOME/.config/nix/configs/bmux/bmux.toml)
  --output-dir DIR    artifact directory (default: target/flicker-repro/<timestamp>)
  --viewport COLSxROWS
                      playbook viewport (default: 180x54)
  --image-protocol PROTOCOL
                      headless real-attach image protocol: kitty, sixel, iterm2, env, none
                      (default: kitty)
  --skip-build        use the existing target/debug/bmux binary
  --no-gif            skip GIF export
  -h, --help          show this help
EOF
}

CONFIG_FILE="${HOME}/.config/nix/configs/bmux/bmux.toml"
OUTPUT_DIR=""
VIEWPORT="180x54"
IMAGE_PROTOCOL="kitty"
SKIP_BUILD=0
EXPORT_GIF=1

while [[ $# -gt 0 ]]; do
  case "$1" in
    --config) CONFIG_FILE="${2:?missing --config value}"; shift 2 ;;
    --output-dir) OUTPUT_DIR="${2:?missing --output-dir value}"; shift 2 ;;
    --viewport) VIEWPORT="${2:?missing --viewport value}"; shift 2 ;;
    --image-protocol) IMAGE_PROTOCOL="${2:?missing --image-protocol value}"; shift 2 ;;
    --skip-build) SKIP_BUILD=1; shift ;;
    --no-gif) EXPORT_GIF=0; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "error: unknown argument: $1" >&2; usage >&2; exit 2 ;;
  esac
done

if [[ ! -f "$CONFIG_FILE" ]]; then
  echo "error: config file not found: $CONFIG_FILE" >&2
  exit 2
fi
case "$IMAGE_PROTOCOL" in
  kitty|sixel|iterm2|env|none) ;;
  *) echo "error: unsupported --image-protocol: $IMAGE_PROTOCOL" >&2; exit 2 ;;
esac

case "$VIEWPORT" in
  *x*) ;;
  *) echo "error: --viewport must be COLSxROWS, got: $VIEWPORT" >&2; exit 2 ;;
esac
COLS="${VIEWPORT%x*}"
ROWS="${VIEWPORT#*x}"
if ! [[ "$COLS" =~ ^[0-9]+$ && "$ROWS" =~ ^[0-9]+$ ]]; then
  echo "error: --viewport must be COLSxROWS, got: $VIEWPORT" >&2
  exit 2
fi

if [[ -z "$OUTPUT_DIR" ]]; then
  OUTPUT_DIR="target/flicker-repro/$(date -u +%Y%m%dT%H%M%SZ)"
fi
mkdir -p "$OUTPUT_DIR"

BMUX_BIN="target/debug/bmux"
if [[ "$SKIP_BUILD" -eq 0 ]]; then
  echo "==> building bmux_cli with bundled decoration + image protocols" >&2
  cargo build -p bmux_cli --features bundled-plugin-decoration,image-protocols
fi
if [[ ! -x "$BMUX_BIN" ]]; then
  echo "error: bmux binary not found/executable: $BMUX_BIN" >&2
  exit 2
fi

CONFIG_COPY="$OUTPUT_DIR/config-used.toml"
PLAYBOOK="$OUTPUT_DIR/playbook.dsl"
RESULT_JSON="$OUTPUT_DIR/playbook.json"
RUN_LOG="$OUTPUT_DIR/run.log"
PERF_JSON="$OUTPUT_DIR/perf.json"
RENDER_TRACE_JSON="$OUTPUT_DIR/render-trace.json"
DISPLAY_TRACK_DIR="$OUTPUT_DIR/display-tracks"
KITTY_HAZARDS="$OUTPUT_DIR/kitty-delete-hazards.txt"
GIF_PATH="$OUTPUT_DIR/repro.gif"
METADATA_JSON="$OUTPUT_DIR/metadata.json"
cp "$CONFIG_FILE" "$CONFIG_COPY"

cat > "$PLAYBOOK" <<EOF
@name decoration-flicker-real-attach
@description Production real-attach decoration flicker repro using copied user config/theme.
@driver real-attach
@viewport cols=$COLS rows=$ROWS
@timeout 45000
@record true
@render-trace true
@sandbox-config $CONFIG_COPY

new-session name=flicker-repro
sleep ms=500
send-keys pane=1 keys='printf "pane 1 foreground\\n"\\r'
sleep ms=250
split-pane direction=vertical
sleep ms=700
send-keys pane=2 keys='printf "pane 2 unfocused pong target\\n"\\r'
sleep ms=250
split-pane direction=horizontal
sleep ms=700
send-keys pane=2 keys='printf "pane 2 second update after split\\n"\\r'
sleep ms=250
render-mark id='after-layout'
focus-pane target=1
sleep ms=400
focus-pane target=2
sleep ms=400
focus-pane target=1
sleep ms=400
resize-viewport cols=$((COLS - 6)) rows=$ROWS
sleep ms=500
resize-viewport cols=$COLS rows=$ROWS
sleep ms=1200
snapshot id=final
assert-render since='after-layout' max_full_frame_frames=20
EOF

args=(playbook run "$PLAYBOOK" --json --record --viewport "$VIEWPORT" --timeout 45 --verbose)
if [[ "$EXPORT_GIF" -eq 1 ]]; then
  args+=(--export-gif "$GIF_PATH")
fi

set +e
if [[ "$IMAGE_PROTOCOL" == "none" ]]; then
  BMUX_PLAYBOOK_HEADLESS_IMAGE_PROTOCOL= "$BMUX_BIN" "${args[@]}" > "$RESULT_JSON" 2> "$RUN_LOG"
else
  BMUX_PLAYBOOK_HEADLESS_IMAGE_PROTOCOL="$IMAGE_PROTOCOL" "$BMUX_BIN" "${args[@]}" > "$RESULT_JSON" 2> "$RUN_LOG"
fi
status=$?
set -e

recording_id=""
recording_path=""
if python3 - <<'PY' "$RESULT_JSON" >/tmp/bmux-repro-recording.tsv 2>/dev/null
import json, sys
text = open(sys.argv[1], 'r', encoding='utf-8').read()
start = text.find('{')
if start < 0:
    raise SystemExit(1)
data = json.loads(text[start:])
print(data.get('recording_id') or '')
print(data.get('recording_path') or '')
PY
then
  mapfile -t recording_lines < /tmp/bmux-repro-recording.tsv
  recording_id="${recording_lines[0]:-}"
  recording_path="${recording_lines[1]:-}"
fi
rm -f /tmp/bmux-repro-recording.tsv

if [[ -n "$recording_id" ]]; then
  "$BMUX_BIN" recording analyze "$recording_id" --perf --json > "$PERF_JSON" 2>> "$RUN_LOG" || printf '{}\n' > "$PERF_JSON"
else
  printf '{}\n' > "$PERF_JSON"
fi

python3 - <<'PY' "$RESULT_JSON" "$RENDER_TRACE_JSON"
import json, sys
text = open(sys.argv[1], 'r', encoding='utf-8').read()
start = text.find('{')
if start < 0:
    raise SystemExit(1)
data = json.loads(text[start:])
trace = [
    {
        "index": step.get("index"),
        "action": step.get("action"),
        "status": step.get("status"),
        "elapsed_ms": step.get("elapsed_ms"),
        "render_summary": step.get("render_summary"),
    }
    for step in data.get("steps", [])
    if step.get("render_summary") is not None
]
with open(sys.argv[2], 'w', encoding='utf-8') as f:
    json.dump(trace, f, indent=2)
    f.write('\n')
PY

if [[ -n "$recording_path" && -d "$recording_path" ]]; then
  mkdir -p "$DISPLAY_TRACK_DIR"
  find "$recording_path" -maxdepth 1 -name 'display-*.bin' -exec cp {} "$DISPLAY_TRACK_DIR" \;
  cp "$recording_path/manifest.json" "$OUTPUT_DIR/recording-manifest.json" 2>/dev/null || true
fi

python3 - <<'PY' "$DISPLAY_TRACK_DIR" "$KITTY_HAZARDS" "$RENDER_TRACE_JSON" "$IMAGE_PROTOCOL"
import json, os, re, sys
track_dir, report_path, render_trace_path, image_protocol = sys.argv[1:]
kitty_re = re.compile(rb'\x1b_G')
place_re = re.compile(rb'\x1b_Ga=p,[^\x1b\x07]*')
delete_re = re.compile(rb'\x1b_Ga=d,[^\x1b\x07]*')
field_re = re.compile(rb'(?:^|,)([icrp])=([0-9]+)')
delete_kind_re = re.compile(rb'(?:^|,)d=([a-z])')

def fields(payload):
    return {key.decode(): int(value) for key, value in field_re.findall(payload)}

def delete_kind(payload):
    match = delete_kind_re.search(payload)
    return match.group(1).decode() if match else '?'

def render_trace_terminal_graphic_totals(path):
    totals = {
        'terminal_graphic_transmits': 0,
        'terminal_graphic_places': 0,
        'terminal_graphic_deletes': 0,
        'terminal_graphic_bytes': 0,
    }
    if not os.path.exists(path):
        return totals
    with open(path, 'r', encoding='utf-8') as f:
        trace = json.load(f)
    for step in trace:
        summary = step.get('render_summary') or {}
        for key in totals:
            totals[key] += int(summary.get(key) or 0)
    return totals

deletes = []
kitty_commands = 0
if os.path.isdir(track_dir):
    for name in sorted(os.listdir(track_dir)):
        if not name.endswith('.bin'):
            continue
        path = os.path.join(track_dir, name)
        data = open(path, 'rb').read()
        kitty_commands += len(kitty_re.findall(data))
        events = []
        for match in place_re.finditer(data):
            events.append((match.start(), 'place', match.group(0)))
        for match in delete_re.finditer(data):
            events.append((match.start(), 'delete', match.group(0)))
        events.sort(key=lambda item: item[0])
        image_positions = {}
        for offset, kind, payload in events:
            f = fields(payload)
            if kind == 'place':
                image_id = f.get('i')
                placement_id = f.get('p')
                if image_id is not None:
                    image_positions[(image_id, placement_id)] = (f.get('c'), f.get('r'))
            elif kind == 'delete':
                image_id = f.get('i')
                placement_id = f.get('p')
                col, row = image_positions.get((image_id, placement_id), (None, None))
                deletes.append({
                    'track': name,
                    'offset': offset,
                    'kind': delete_kind(payload),
                    'image_id': image_id,
                    'placement_id': placement_id,
                    'col': col,
                    'row': row,
                })

graphic_totals = render_trace_terminal_graphic_totals(render_trace_path)
inconclusive = (
    image_protocol == 'kitty'
    and kitty_commands == 0
    and graphic_totals['terminal_graphic_transmits'] == 0
    and graphic_totals['terminal_graphic_places'] == 0
)

with open(report_path, 'w', encoding='utf-8') as out:
    out.write(f"Kitty commands in display tracks: {kitty_commands}\n")
    out.write(
        "Render trace terminal graphics: "
        f"transmits={graphic_totals['terminal_graphic_transmits']} "
        f"places={graphic_totals['terminal_graphic_places']} "
        f"deletes={graphic_totals['terminal_graphic_deletes']} "
        f"bytes={graphic_totals['terminal_graphic_bytes']}\n"
    )
    if inconclusive:
        out.write('No Kitty graphics emitted; delete audit is inconclusive\n')
    if deletes:
        out.write('Kitty delete commands detected\n')
        for delete in deletes[:100]:
            out.write(
                f"{delete['track']}: delete@{delete['offset']} kind={delete['kind']} "
                f"image={delete['image_id']} placement={delete['placement_id']} "
                f"last_size_cols={delete['col']} last_size_rows={delete['row']}\n"
            )
        if len(deletes) > 100:
            out.write(f'... {len(deletes) - 100} more\n')
    else:
        out.write('No Kitty delete commands detected\n')
if inconclusive:
    sys.exit(3)
PY

python3 - <<'PY' "$METADATA_JSON" "$CONFIG_FILE" "$CONFIG_COPY" "$PLAYBOOK" "$RESULT_JSON" "$RUN_LOG" "$PERF_JSON" "$RENDER_TRACE_JSON" "$DISPLAY_TRACK_DIR" "$KITTY_HAZARDS" "$GIF_PATH" "$recording_id" "$recording_path" "$VIEWPORT" "$IMAGE_PROTOCOL" "$status"
import json, os, sys
(
    out, config_file, config_copy, playbook, result_json, run_log, perf_json,
    render_trace_json, display_track_dir, kitty_hazards, gif_path, recording_id,
    recording_path, viewport, image_protocol, status,
) = sys.argv[1:]
metadata = {
    "status": int(status),
    "viewport": viewport,
    "image_protocol": image_protocol,
    "config_source": config_file,
    "config_used": config_copy,
    "playbook": playbook,
    "result_json": result_json,
    "run_log": run_log,
    "perf_json": perf_json,
    "render_trace_json": render_trace_json,
    "display_track_dir": display_track_dir if os.path.isdir(display_track_dir) else None,
    "kitty_hazards": kitty_hazards if os.path.exists(kitty_hazards) else None,
    "gif": gif_path if os.path.exists(gif_path) else None,
    "recording_id": recording_id or None,
    "recording_path": recording_path or None,
}
with open(out, 'w', encoding='utf-8') as f:
    json.dump(metadata, f, indent=2)
    f.write('\n')
PY

echo "==> artifacts: $OUTPUT_DIR" >&2
echo "    result:    $RESULT_JSON" >&2
echo "    log:       $RUN_LOG" >&2
echo "    metadata:  $METADATA_JSON" >&2
echo "    trace:     $RENDER_TRACE_JSON" >&2
echo "    hazards:   $KITTY_HAZARDS" >&2
if [[ -d "$DISPLAY_TRACK_DIR" ]]; then
  echo "    displays:  $DISPLAY_TRACK_DIR" >&2
fi
if [[ -f "$GIF_PATH" ]]; then
  echo "    gif:       $GIF_PATH" >&2
fi
if [[ -n "$recording_path" ]]; then
  echo "    recording: $recording_path" >&2
fi

exit "$status"
