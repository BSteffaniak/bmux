#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: scripts/perf-audit.sh [--quick|--full] [--target-server] [--require-perf-events]
                             [--output-dir DIR] [--playbook-dir DIR] [--budget-dir DIR]
                             [--keep-going]

Runs bmux perf playbooks, records each run, analyzes recording perf telemetry
when available, and evaluates scenario budgets.

Artifacts are written to target/perf-audit/<timestamp>/ by default.
EOF
}

MODE="quick"
TARGET_SERVER=0
REQUIRE_PERF_EVENTS=0
KEEP_GOING=0
PLAYBOOK_DIR="tests/perf/playbooks"
BUDGET_DIR="tests/perf/budgets"
OUTPUT_DIR=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --quick) MODE="quick"; shift ;;
    --full) MODE="full"; shift ;;
    --target-server) TARGET_SERVER=1; shift ;;
    --require-perf-events) REQUIRE_PERF_EVENTS=1; shift ;;
    --keep-going) KEEP_GOING=1; shift ;;
    --output-dir) OUTPUT_DIR="${2:?missing --output-dir value}"; shift 2 ;;
    --playbook-dir) PLAYBOOK_DIR="${2:?missing --playbook-dir value}"; shift 2 ;;
    --budget-dir) BUDGET_DIR="${2:?missing --budget-dir value}"; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "error: unknown argument: $1" >&2; usage >&2; exit 2 ;;
  esac
done

command -v bmux >/dev/null || { echo "error: bmux not found on PATH" >&2; exit 2; }
command -v jq >/dev/null || { echo "error: jq not found on PATH" >&2; exit 2; }

if [[ ! -d "$PLAYBOOK_DIR" ]]; then
  echo "error: playbook dir not found: $PLAYBOOK_DIR" >&2
  exit 2
fi
if [[ ! -d "$BUDGET_DIR" ]]; then
  echo "error: budget dir not found: $BUDGET_DIR" >&2
  exit 2
fi

if [[ -z "$OUTPUT_DIR" ]]; then
  OUTPUT_DIR="target/perf-audit/$(date -u +%Y%m%dT%H%M%SZ)"
fi
mkdir -p "$OUTPUT_DIR"
SUMMARY_TSV="$OUTPUT_DIR/summary.tsv"
printf 'scenario\tstatus\ttotal_ms\tframes\tfull_frames\trows\tcells\tframe_bytes\tgfx_tx\tgfx_place\tgfx_delete\tgfx_bytes\tperf_events\trender_p95_ms\tdrain_p95_ms\tartifact\n' > "$SUMMARY_TSV"

failures=0
runs=0

for playbook in "$PLAYBOOK_DIR"/*.dsl; do
  [[ -e "$playbook" ]] || continue
  scenario="$(basename "$playbook" .dsl)"
  if [[ "$MODE" == "quick" && "$scenario" == full-* ]]; then
    continue
  fi
  runs=$((runs + 1))
  scenario_dir="$OUTPUT_DIR/$scenario"
  mkdir -p "$scenario_dir"
  result_json="$scenario_dir/playbook.json"
  perf_json="$scenario_dir/perf.json"
  metrics_json="$scenario_dir/metrics.json"
  violations_json="$scenario_dir/violations.json"
  recommendations_json="$scenario_dir/recommendations.json"
  log_file="$scenario_dir/run.log"
  budget_json="$BUDGET_DIR/$scenario.json"
  if [[ ! -f "$budget_json" ]]; then
    echo "warning: missing budget for $scenario; using empty budget" | tee -a "$log_file" >&2
    printf '{}\n' > "$scenario_dir/budget.empty.json"
    budget_json="$scenario_dir/budget.empty.json"
  fi

  echo "==> $scenario" >&2
  run_status=0
  args=(playbook run "$playbook" --json --record)
  if [[ "$TARGET_SERVER" -eq 1 ]]; then
    args+=(--target-server)
  fi
  if BMUX_PLAYBOOK_PERF_RECORDING_LEVEL="${BMUX_PLAYBOOK_PERF_RECORDING_LEVEL:-trace}" \
    BMUX_PLAYBOOK_PERF_WINDOW_MS="${BMUX_PLAYBOOK_PERF_WINDOW_MS:-100}" \
    bmux "${args[@]}" > "$result_json" 2> "$log_file"; then
    run_status=0
  else
    run_status=$?
  fi

  recording_id=""
  if jq -e . "$result_json" >/dev/null 2>&1; then
    recording_id="$(jq -r '.recording_id // empty' "$result_json")"
  else
    mv "$result_json" "$result_json.invalid"
    printf '{}\n' > "$result_json"
  fi

  if [[ -n "$recording_id" ]]; then
    if ! bmux recording analyze "$recording_id" --perf --json > "$perf_json" 2>> "$log_file"; then
      echo "warning: perf analysis failed for $scenario recording=$recording_id" >> "$log_file"
      printf '{}\n' > "$perf_json"
    fi
  else
    printf '{}\n' > "$perf_json"
  fi

  jq -n \
    --arg scenario "$scenario" \
    --arg playbook "$playbook" \
    --arg result_path "$result_json" \
    --arg perf_path "$perf_json" \
    --arg log_path "$log_file" \
    --argjson run_status "$run_status" \
    --slurpfile r "$result_json" \
    --slurpfile p "$perf_json" \
    --slurpfile b "$budget_json" '
      def n($x): ($x // 0);
      ($r[0]) as $r |
      ($p[0]) as $p |
      ($b[0]) as $b |
      ($b.ignore_actions // []) as $ignore |
      (($r.steps // []) | map(select((.action as $a | ($ignore | index($a) | not))))) as $steps |
      ($steps | map(.render_summary // {})) as $summaries |
      def sum_field($k): ($summaries | map(n(.[$k])) | add // 0);
      def max_field($k): ($summaries | map(n(.[$k])) | max // 0);
      {
        scenario: $scenario,
        playbook: $playbook,
        result_path: $result_path,
        perf_path: $perf_path,
        log_path: $log_path,
        budget_description: ($b.description // null),
        run_status: $run_status,
        pass: (($run_status == 0) and ($r.pass == true)),
        recording_id: ($r.recording_id // null),
        recording_path: ($r.recording_path // null),
        total_elapsed_ms: n($r.total_elapsed_ms),
        step_count: (($r.steps // []) | length),
        evaluated_step_count: ($steps | length),
        max_step_elapsed_ms: ($steps | map(n(.elapsed_ms)) | max // 0),
        render: {
          frames: sum_field("frames"),
          max_frames_per_step: max_field("frames"),
          full_frame_frames: sum_field("full_frame_frames"),
          full_surface_fallbacks: sum_field("full_surface_fallbacks"),
          damage_rects: sum_field("damage_rects"),
          damage_area_cells: sum_field("damage_area_cells"),
          rows_emitted: sum_field("rows_emitted"),
          row_segments_emitted: sum_field("row_segments_emitted"),
          cells_emitted: sum_field("cells_emitted"),
          frame_bytes: sum_field("frame_bytes"),
          max_frame_bytes_per_step: max_field("frame_bytes"),
          status_rendered_frames: sum_field("status_rendered_frames"),
          overlay_rendered_frames: sum_field("overlay_rendered_frames"),
          terminal_graphic_transmits: sum_field("terminal_graphic_transmits"),
          terminal_graphic_places: sum_field("terminal_graphic_places"),
          terminal_graphic_deletes: sum_field("terminal_graphic_deletes"),
          terminal_graphic_bytes: sum_field("terminal_graphic_bytes")
        },
        perf: {
          perf_events: n($p.perf_events),
          malformed_payloads: n($p.malformed_payloads),
          dropped_events_reported: n($p.dropped_events_reported),
          dropped_payload_bytes_reported: n($p.dropped_payload_bytes_reported),
          connect_to_first_frame_ms: n($p.connect_to_first_frame_ms),
          connect_to_interactive_ms: n($p.connect_to_interactive_ms),
          reconnect_outage_max_ms: n($p.reconnect_outage_max_ms),
          render_p95_ms: n($p.timings_ms.render_ms_max.p95_ms),
          drain_ipc_p95_ms: n($p.timings_ms.drain_ipc_ms_max.p95_ms),
          attach_window_counters: ($p.attach_window_counters // {}),
          render_outliers: ($p.render_outliers // []),
          full_surface_render_outliers_after_ms: (($p.render_outliers // []) | map(select(
            ((.since_attach_start_ms // 0) >= (($b.perf.full_surface_outlier_after_ms // $b.perf.outlier_after_ms // 0)))
            and (((.full_frame_fallback // false) == true) or (((.extension_stats // {}) | to_entries | map(.value.full_surface_calls // 0) | add // 0) > 0))
          )) | length),
          extension_full_surface_calls_after_ms: (($p.render_outliers // []) | map(select(
            ((.since_attach_start_ms // 0) >= (($b.perf.full_surface_outlier_after_ms // $b.perf.outlier_after_ms // 0)))
          ) | ((.extension_stats // {}) | to_entries | map(.value.full_surface_calls // 0) | add // 0)) | add // 0),
          hints: ($p.hints // []),
          outlier_samples: ($p.outlier_samples // [])
        }
      }
    ' > "$metrics_json"

  jq -n --argjson require_perf_events "$REQUIRE_PERF_EVENTS" --slurpfile m "$metrics_json" --slurpfile b "$budget_json" '
    def check_max($name; $actual; $max):
      if $max == null then empty
      elif $actual <= $max then empty
      else {kind:"max", name:$name, actual:$actual, expected:$max}
      end;
    def check_bool($name; $actual; $expected):
      if $expected == null then empty
      elif $actual == $expected then empty
      else {kind:"bool", name:$name, actual:$actual, expected:$expected}
      end;
    ($m[0]) as $m |
    ($b[0]) as $b |
    ($b.render // {}) as $r |
    ($b.perf // {}) as $p |
    [
      check_bool("playbook.pass"; $m.pass; true),
      check_max("total_elapsed_ms"; $m.total_elapsed_ms; $b.max_total_elapsed_ms),
      check_max("max_step_elapsed_ms"; $m.max_step_elapsed_ms; $b.max_step_elapsed_ms),
      check_max("render.frames"; $m.render.frames; $r.max_frames),
      check_max("render.max_frames_per_step"; $m.render.max_frames_per_step; $r.max_frames_per_step),
      check_max("render.full_frame_frames"; $m.render.full_frame_frames; $r.max_full_frame_frames),
      check_max("render.full_surface_fallbacks"; $m.render.full_surface_fallbacks; $r.max_full_surface_fallbacks),
      check_max("render.damage_rects"; $m.render.damage_rects; $r.max_damage_rects),
      check_max("render.damage_area_cells"; $m.render.damage_area_cells; $r.max_damage_area_cells),
      check_max("render.rows_emitted"; $m.render.rows_emitted; $r.max_rows_emitted),
      check_max("render.row_segments_emitted"; $m.render.row_segments_emitted; $r.max_row_segments_emitted),
      check_max("render.cells_emitted"; $m.render.cells_emitted; $r.max_cells_emitted),
      check_max("render.frame_bytes"; $m.render.frame_bytes; $r.max_frame_bytes),
      check_max("render.max_frame_bytes_per_step"; $m.render.max_frame_bytes_per_step; $r.max_frame_bytes_per_step),
      check_max("render.terminal_graphic_transmits"; $m.render.terminal_graphic_transmits; $r.max_terminal_graphic_transmits),
      check_max("render.terminal_graphic_places"; $m.render.terminal_graphic_places; $r.max_terminal_graphic_places),
      check_max("render.terminal_graphic_deletes"; $m.render.terminal_graphic_deletes; $r.max_terminal_graphic_deletes),
      check_max("render.terminal_graphic_bytes"; $m.render.terminal_graphic_bytes; $r.max_terminal_graphic_bytes),
      (if (($p.require_events // false) or ($require_perf_events == 1)) and ($m.perf.perf_events <= 0)
       then {kind:"min", name:"perf.perf_events", actual:$m.perf.perf_events, expected:">0"}
       else empty end),
      check_max("perf.malformed_payloads"; $m.perf.malformed_payloads; $p.max_malformed_payloads),
      check_max("perf.dropped_events_reported"; $m.perf.dropped_events_reported; $p.max_dropped_events_reported),
      check_max("perf.dropped_payload_bytes_reported"; $m.perf.dropped_payload_bytes_reported; $p.max_dropped_payload_bytes_reported),
      check_max("perf.connect_to_interactive_ms"; $m.perf.connect_to_interactive_ms; $p.max_connect_to_interactive_ms),
      check_max("perf.reconnect_outage_max_ms"; $m.perf.reconnect_outage_max_ms; $p.max_reconnect_outage_max_ms),
      (if $m.perf.perf_events > 0 then check_max("perf.render_p95_ms"; $m.perf.render_p95_ms; $p.max_render_p95_ms) else empty end),
      (if $m.perf.perf_events > 0 then check_max("perf.drain_ipc_p95_ms"; $m.perf.drain_ipc_p95_ms; $p.max_drain_ipc_p95_ms) else empty end),
      (if $m.perf.perf_events > 0 then check_max("perf.full_surface_render_outliers_after_ms"; $m.perf.full_surface_render_outliers_after_ms; $p.max_full_surface_render_outliers_after_ms) else empty end),
      (if $m.perf.perf_events > 0 then check_max("perf.extension_full_surface_calls_after_ms"; $m.perf.extension_full_surface_calls_after_ms; $p.max_extension_full_surface_calls_after_ms) else empty end)
    ]
  ' > "$violations_json"

  jq -n --slurpfile m "$metrics_json" --slurpfile v "$violations_json" '
    def rec($severity; $category; $message; $next):
      {severity:$severity, category:$category, message:$message, next:$next};
    ($m[0]) as $m |
    ($v[0]) as $v |
    [
      ($v[]? | rec("error"; "budget"; "budget violation: \(.name) actual=\(.actual) expected=\(.expected)"; "Open metrics.json and compare this scenario against its budget.")),
      (if $m.perf.perf_events == 0 then
        rec("error"; "instrumentation"; "recording contains no bmux.perf custom events"; "Use a real attach-driven scenario and ensure recordings include custom events with performance recording level trace/detailed.")
       else empty end),
      ($m.perf.hints[]? | rec("warning"; "telemetry"; .; "Inspect perf.json outliers and attach_window_counters for the matching scenario.")),
      (if ($m.render.terminal_graphic_deletes // 0) > 0 then
        rec("warning"; "graphics"; "playbook render summary observed retained graphics deletes"; "Check whether deletes are limited to expected layout/tab teardown, not steady-state focus/hover.")
       else empty end),
      (if (($m.perf.attach_window_counters.terminal_graphic_transmits // 0) > 0 and ($m.perf.attach_window_counters.terminal_graphic_deletes // 0) > 0) then
        rec("warning"; "graphics"; "real attach telemetry observed graphics transmit/delete churn"; "Inspect retained graphics keys/signatures and damage-triggered reconciliation.")
       else empty end),
      (if (($m.perf.attach_window_counters.full_frame_fallbacks // 0) > 0) then
        rec("warning"; "render"; "real attach telemetry observed full-frame fallback"; "Inspect dirty reason promotion and damage coalescing around this scenario.")
       else empty end)
    ]
  ' > "$recommendations_json"

  violation_count="$(jq 'length' "$violations_json")"
  if [[ "$violation_count" -eq 0 ]]; then
    status="PASS"
  else
    status="FAIL"
    failures=$((failures + 1))
  fi

  jq -r --arg status "$status" --arg artifact "$scenario_dir" '
    [
      .scenario,
      $status,
      .total_elapsed_ms,
      .render.frames,
      .render.full_frame_frames,
      .render.rows_emitted,
      .render.cells_emitted,
      .render.frame_bytes,
      .render.terminal_graphic_transmits,
      .render.terminal_graphic_places,
      .render.terminal_graphic_deletes,
      .render.terminal_graphic_bytes,
      .perf.perf_events,
      .perf.render_p95_ms,
      .perf.drain_ipc_p95_ms,
      $artifact
    ] | @tsv
  ' "$metrics_json" >> "$SUMMARY_TSV"

  recommendation_count="$(jq 'length' "$recommendations_json")"
  if [[ "$recommendation_count" -gt 0 ]]; then
    echo "recommendations for $scenario:" >&2
    jq -r '.[] | "  - [\(.severity)] \(.category): \(.message)"' "$recommendations_json" >&2
  fi

  if [[ "$status" == "FAIL" ]]; then
    echo "FAIL $scenario — violations:" >&2
    jq -r '.[] | "  - \(.name): actual=\(.actual) expected=\(.expected)"' "$violations_json" >&2
    if [[ "$KEEP_GOING" -ne 1 ]]; then
      break
    fi
  fi
done

printf '\nBMUX perf audit summary (%s)\n' "$MODE"
printf 'Artifacts: %s\n\n' "$OUTPUT_DIR"
column -t -s $'\t' "$SUMMARY_TSV" || cat "$SUMMARY_TSV"

if [[ "$runs" -eq 0 ]]; then
  echo "error: no playbooks matched in $PLAYBOOK_DIR" >&2
  exit 2
fi
if [[ "$failures" -ne 0 ]]; then
  printf '\nPerf audit failed: %s scenario(s) exceeded budget.\n' "$failures" >&2
  exit 1
fi

printf '\nPerf audit passed: %s scenario(s).\n' "$runs"
