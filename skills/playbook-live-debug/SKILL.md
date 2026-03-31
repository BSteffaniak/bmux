---
name: playbook-live-debug
description: Use bmux live playbook as a headless, token-efficient debugging engine for realtime troubleshooting.
---

# playbook-live-debug

Use bmux live playbook as a headless, token-efficient debugging engine for LLM-driven troubleshooting.

## When to use this skill

Use this skill when you need to:

- Reproduce a runtime issue interactively in bmux.
- Observe terminal behavior in real time (output, cursor, input, server events, request lifecycle).
- Detect anomalies quickly with watchpoints.
- Hydrate only relevant evidence windows for analysis.
- Produce evidence-backed diagnosis with sequence references.

Do not use this skill for static code review with no runtime behavior.

---

## Core principles

- JSON-op only workflow for interactive mode.
- Start with narrow subscriptions and tight budgets.
- Use watchpoints to detect incidents instead of polling full screen repeatedly.
- Hydrate evidence windows only after a signal (`watchpoint_hit`).
- Keep analysis tied to `seq`/`mono_ns` timeline.

---

## Event model quick reference

Common event types:

- `pane_output`
- `pane_input`
- `cursor_delta`
- `screen_delta`
- `server_event`
- `request_lifecycle`
- `watchpoint_hit`

Hydration modes:

- `screen_full`
- `event_window`
- `incident`

---

## Fast-start workflow

1. Start interactive playbook session.
2. Connect to socket and send `hello`.
3. Create/attach session via `command` DSL.
4. Subscribe with low-token defaults.
5. Configure watchpoints for expected anomaly channels.
6. Execute repro command(s).
7. On `watchpoint_hit`, hydrate `incident`.
8. If needed, hydrate `event_window` with tighter range.
9. Summarize root cause hypothesis with evidence seqs.
10. Propose and run minimal confirmatory command(s).

---

## Canonical JSON ops

### Handshake

```json
{
  "op": "hello",
  "protocol_version": 1,
  "client": "llm-agent",
  "prefer_machine_readable": true
}
```

### Create session

```json
{ "op": "command", "request_id": "r-new", "dsl": "new-session" }
```

### LLM-first subscription (token-efficient)

```json
{
  "op": "subscribe",
  "request_id": "r-sub",
  "event_types": [
    "cursor_delta",
    "screen_delta",
    "request_lifecycle",
    "watchpoint_hit"
  ],
  "pane_indexes": [1],
  "screen_delta_format": "line_ops",
  "max_events_per_sec": 120,
  "max_bytes_per_sec": 65536,
  "coalesce_ms": 40
}
```

### Add pane output only when needed

```json
{
  "op": "subscribe",
  "request_id": "r-sub-output",
  "event_types": [
    "pane_output",
    "cursor_delta",
    "screen_delta",
    "request_lifecycle",
    "watchpoint_hit"
  ],
  "pane_indexes": [1],
  "screen_delta_format": "line_ops",
  "max_events_per_sec": 160,
  "max_bytes_per_sec": 131072,
  "coalesce_ms": 30
}
```

### Generic burst watchpoint

```json
{
  "op": "set_watchpoint",
  "request_id": "r-wp-cursor",
  "id": "cursor-burst",
  "kind": "event_burst",
  "event_type": "cursor_delta",
  "pane_index": 1,
  "min_hits": 3,
  "window_ms": 250
}
```

### Output regex watchpoint

```json
{
  "op": "set_watchpoint",
  "request_id": "r-wp-output",
  "id": "error-output",
  "kind": "event_burst",
  "event_type": "pane_output",
  "pane_index": 1,
  "contains_regex": "(?i)error|panic|traceback",
  "min_hits": 1,
  "window_ms": 1000
}
```

### Run repro command

```json
{
  "op": "command",
  "request_id": "r-repro",
  "dsl": "send-keys keys='nvim somefile.rs\\r'"
}
```

### Hydrate around latest incident by watchpoint id

```json
{
  "op": "hydrate",
  "request_id": "r-inc",
  "kind": "incident",
  "id": "cursor-burst",
  "window_radius": 80
}
```

### Hydrate explicit seq window

```json
{
  "op": "hydrate",
  "request_id": "r-win",
  "kind": "event_window",
  "start_seq": 1200,
  "end_seq": 1320
}
```

### Hydrate full screen snapshot

```json
{
  "op": "hydrate",
  "request_id": "r-screen",
  "kind": "screen_full",
  "pane_index": 1
}
```

### Cleanup

```json
{"op":"unsubscribe","request_id":"r-unsub"}
{"op":"quit","request_id":"r-quit"}
```

---

## Investigation playbook

### A) Repro

- Issue one focused repro command at a time.
- Prefer deterministic commands over ad-hoc manual input.
- Track request lifecycle events for success/error timing context.

### B) Detect

- Use `event_burst` watchpoints for noisy or intermittent issues.
- For cursor jitter, prioritize `cursor_delta` + `screen_delta`.
- For command failures, prioritize `request_lifecycle` + `pane_output` regex.

### C) Hydrate

- First call `hydrate incident` using watchpoint id.
- If insufficient, call `hydrate event_window` with tighter seq range.
- Use `screen_full` only when a full viewport snapshot is required.

### D) Diagnose

- Correlate:
  - request start/done/error
  - cursor movement bursts
  - screen patch bursts
  - server events around same seq band
  - input immediately preceding anomaly

### E) Confirm

- Run one confirming command per hypothesis.
- Re-check with targeted watchpoint or narrow event window.
- Avoid broad replays unless new evidence contradicts hypothesis.

---

## nvim cursor-jump recipe

1. `new-session`
2. subscribe to `cursor_delta`, `screen_delta`, `request_lifecycle`, `watchpoint_hit`
3. set watchpoint:
   - `id=cursor-burst`
   - `event_type=cursor_delta`
   - `min_hits=3`, `window_ms=250`
4. `send-keys 'nvim <file>\\r'`
5. wait for `watchpoint_hit` on `cursor-burst`
6. `hydrate incident id=cursor-burst window_radius=100`
7. inspect ordering:
   - did burst align with startup redraw?
   - was there concurrent request error?
   - did server event indicate attach/view churn?
8. issue one confirmatory command and compare windows.

---

## Token-efficiency defaults

Use these defaults unless debugging is blocked:

- `screen_delta_format`: `line_ops`
- `event_types`: start with `cursor_delta`, `screen_delta`, `request_lifecycle`, `watchpoint_hit`
- `max_events_per_sec`: `120`
- `max_bytes_per_sec`: `65536`
- `coalesce_ms`: `40`
- Hydration window radius: `50-120` seqs

Escalate only when needed:

- Add `pane_output` for textual clues.
- Add `server_event` for runtime/system-level context.
- Increase budgets temporarily, then reduce again.

---

## Guardrails

- Do not watch `watchpoint_hit` with watchpoints (blocked).
- Do not rely on fixed sleeps for assertions; prefer `wait-for` or watchpoint+hydrate.
- Do not hydrate huge windows by default.
- Do not emit conclusions without citing concrete `seq` evidence bands.
- Keep one active hypothesis per loop to prevent token sprawl.

---

## Output contract (what to report)

Always return:

1. **Observed behavior** (1-3 bullets)
2. **Likely cause** (single primary hypothesis)
3. **Evidence** (`seq`/`event` references, short timeline)
4. **Confidence** (`high|medium|low` + why)
5. **Next action** (one command or patch direction)
6. **Verification step** (how to prove fix)

Example evidence line:

- `seq 1842-1868`: `request_lifecycle:start(nvim)` followed by `cursor_delta` burst and `screen_delta` burst, no server error events.

---

## Failure handling

If no useful signal appears:

- Widen one dimension only:
  - add one event type OR
  - increase one budget OR
  - widen hydrate window
- Re-run repro once.
- If still empty, report "insufficient runtime signal" and list exactly what telemetry is missing.

---

## Minimal command sequence template

```json
{"op":"hello","protocol_version":1,"client":"llm-agent","prefer_machine_readable":true}
{"op":"command","request_id":"1","dsl":"new-session"}
{"op":"subscribe","request_id":"2","event_types":["cursor_delta","screen_delta","request_lifecycle","watchpoint_hit"],"pane_indexes":[1],"screen_delta_format":"line_ops","max_events_per_sec":120,"max_bytes_per_sec":65536,"coalesce_ms":40}
{"op":"set_watchpoint","request_id":"3","id":"cursor-burst","kind":"event_burst","event_type":"cursor_delta","pane_index":1,"min_hits":3,"window_ms":250}
{"op":"command","request_id":"4","dsl":"send-keys keys='nvim file.rs\\r'"}
{"op":"hydrate","request_id":"5","kind":"incident","id":"cursor-burst","window_radius":80}
{"op":"quit","request_id":"6"}
```
