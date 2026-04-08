# Performance Metrics in Recordings

This document describes bmux performance telemetry captured as recording
`Custom` events (`source = "bmux.perf"`).

## Enable telemetry

In `~/.config/bmux/config.toml`:

```toml
[performance]
recording_level = "detailed" # off | basic | detailed | trace
window_ms = 1000
max_events_per_sec = 32
max_payload_bytes_per_sec = 65536
```

Telemetry requires recordings that include the `custom` event kind.

Use either:

- default recording settings with `recording.capture_events = true`, or
- explicit kinds: `bmux recording start --kind custom ...`

`bmux recording status` warns when perf recording is enabled but defaults do not
capture `custom` events.

## Capture workflow

1. Start a recording (`bmux recording start ...`).
2. Reproduce the latency/jank/reconnect issue.
3. Stop recording (`bmux recording stop`).
4. Analyze telemetry:

```bash
bmux recording analyze <recording-id> --perf
bmux recording analyze <recording-id> --perf --json
```

## Schema contract

All `bmux.perf` payloads include:

- `schema_version` (currently `1`)
- `level` (`off|basic|detailed|trace`)
- `runtime` (`cli` or `server`)
- `ts_epoch_ms`

Rate-limited streams may also include:

- `dropped_events_since_emit`
- `dropped_payload_bytes_since_emit`

## Event catalog

- `iroh.connect.summary`
  - basic: connect and total timings
  - detailed: staged iroh timings (bind/online/open_bi/ipc/etc)
  - trace: endpoint id + configured timeout

- `attach.first_frame`
  - startup latency from attach start to first rendered frame

- `attach.interactive.ready`
  - startup latency from attach start to first hydrated interactive frame

- `attach.window`
  - periodic attach-loop aggregates (drain IPC + render)
  - trace includes additional drain behavior (`drain_budget_hits`, etc)

- `attach.frame.trace`
  - per-frame render timing (trace level)

- `attach.exit`
  - attach lifetime and final exit reason

- `iroh.reconnect.outage`
  - outage duration for reconnect attempts

- `iroh.attach.attempt`
  - attach attempt duration and stream-closed outcome (on reconnect paths)

- `server.push.window`
  - periodic server event-push health (events/bytes/lag)

## Level guidance

- `off`: no telemetry emission.
- `basic`: low-volume summaries for connect/attach/push health.
- `detailed`: adds staged timings and richer window aggregates.
- `trace`: highest detail, including per-frame trace events.

If `recording analyze` reports dropped telemetry counters, increase
`performance.max_events_per_sec` and/or `performance.max_payload_bytes_per_sec`
for deeper captures.
